//! Neutral read APIs. The main loop only reserves a slot; all FS and Git work
//! happens on bounded workers, returning JSON directly without mutating AppState.
use std::io;
use std::path::{Component, Path};
use std::process::Stdio;
use std::time::Duration;

use super::responses::{encode_error, encode_success};
use crate::api::schema::*;
use crate::app::App;

const FILE_BYTES: usize = 64 * 1024;
const LIST_ENTRIES: usize = 256;
const GIT_BYTES: usize = 128 * 1024;
const GIT_TIMEOUT: Duration = Duration::from_secs(3);

impl App {
    pub(crate) fn start_api_read(
        &mut self,
        request: Request,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        let preview_permit = if matches!(request.method, Method::FilePreview(_)) {
            match self.preview_read_slots.clone().try_acquire_owned() {
                Ok(permit) => Some(permit),
                Err(_) => {
                    let _ = respond_to.send(encode_error(
                        request.id,
                        "read_busy",
                        "too many previews pending",
                    ));
                    return;
                }
            }
        } else {
            None
        };
        let runtime = tokio::runtime::Handle::current();
        let id = request.id;
        spawn_read_worker(self.file_read_slots.clone(), id, respond_to, move || {
            let _permit = preview_permit;
            execute_read(request.method, &runtime)
        });
    }
}

fn spawn_read_worker(
    slots: std::sync::Arc<tokio::sync::Semaphore>,
    id: String,
    respond_to: std::sync::mpsc::Sender<String>,
    read: impl FnOnce() -> io::Result<ResponseResult> + Send + 'static,
) {
    let Ok(permit) = slots.try_acquire_owned() else {
        let _ = respond_to.send(encode_error(
            id,
            "read_busy",
            "too many reads pending; retry shortly",
        ));
        return;
    };
    let failure_tx = respond_to.clone();
    let failure_id = id.clone();
    let spawn = std::thread::Builder::new()
        .name("file-git-read".into())
        .spawn(move || {
            let response = match read() {
                Ok(result) => encode_success(id, result),
                Err(error) => {
                    let code = match error.kind() {
                        io::ErrorKind::Unsupported => "unsupported",
                        io::ErrorKind::TimedOut => "read_timeout",
                        io::ErrorKind::InvalidInput => "invalid_request",
                        _ => "read_failed",
                    };
                    encode_error(id, code, error.to_string())
                }
            };
            drop(permit);
            let _ = respond_to.send(response);
        });
    if let Err(error) = spawn {
        let _ = failure_tx.send(encode_error(failure_id, "read_failed", error.to_string()));
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

pub(super) fn root_path(value: &str) -> io::Result<&Path> {
    let path = Path::new(value);
    if !path.is_absolute() || value.len() > 8192 {
        return Err(invalid("absolute server-local root required"));
    }
    Ok(path)
}

pub(super) fn relative_path(value: &str) -> io::Result<&Path> {
    let path = Path::new(value);
    if value.len() > 8192
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(invalid(
            "child path must be relative without dot or parent components",
        ));
    }
    Ok(path)
}

fn execute_read(method: Method, runtime: &tokio::runtime::Handle) -> io::Result<ResponseResult> {
    match method {
        Method::FilePreview(params) => runtime.block_on(super::file_preview::read_preview(params)),
        Method::FileList(params) => {
            let root = root_path(&params.root)?;
            let path = relative_path(&params.path)?;
            let (names, mut truncated) =
                crate::platform::list_confined_directory(root, path, LIST_ENTRIES)?;
            let mut entries = Vec::new();
            let mut path_bytes = 0;
            for entry in names {
                let Some(name) = entry.name.to_str() else {
                    truncated = true;
                    continue;
                };
                path_bytes += params.path.len() + name.len() + 1;
                if path_bytes > FILE_BYTES {
                    truncated = true;
                    break;
                }
                entries.push(FileEntry {
                    path: path
                        .join(name)
                        .to_str()
                        .ok_or_else(|| invalid("non-UTF8 path"))?
                        .to_owned(),
                    directory: entry.directory,
                    symlink: entry.symlink,
                });
            }
            entries.sort_by(|a, b| {
                b.directory
                    .cmp(&a.directory)
                    .then_with(|| a.path.cmp(&b.path))
            });
            Ok(ResponseResult::FileList {
                root: root.canonicalize()?.to_string_lossy().into_owned(),
                entries,
                truncated,
            })
        }
        Method::FileRead(params) => {
            let mut bytes = crate::platform::read_confined_file(
                root_path(&params.root)?,
                relative_path(&params.path)?,
                FILE_BYTES + 1,
            )?;
            let truncated = bytes.len() > FILE_BYTES;
            bytes.truncate(FILE_BYTES);
            if truncated {
                if let Err(error) = std::str::from_utf8(&bytes) {
                    if error.error_len().is_none() {
                        bytes.truncate(error.valid_up_to());
                    }
                }
            }
            let binary = bytes.contains(&0) || std::str::from_utf8(&bytes).is_err();
            let text = if binary {
                String::new()
            } else {
                String::from_utf8_lossy(&bytes).into_owned()
            };
            Ok(ResponseResult::FileRead {
                text,
                binary,
                truncated,
            })
        }
        Method::GitStatus(params) => runtime.block_on(async {
            let root = git_root(&params.cwd).await?;
            let output = git(
                &root,
                &[
                    "status",
                    "--porcelain=v1",
                    "-z",
                    "-b",
                    "--untracked-files=all",
                ],
                GIT_BYTES,
            )
            .await?;
            let (branch, entries, incomplete) = parse_status(&output.bytes)?;
            Ok(ResponseResult::GitStatus {
                root,
                branch,
                entries,
                truncated: output.truncated || incomplete,
            })
        }),
        Method::GitDiff(params) => runtime.block_on(async {
            relative_path(&params.path)?;
            let root = git_root(&params.cwd).await?;
            let mut args = vec!["diff", "--no-ext-diff", "--no-textconv", "--no-color"];
            if params.staged {
                args.push("--cached");
            }
            args.extend(["--", &params.path]);
            if let Some(original) = &params.original_path {
                relative_path(original)?;
                args.push(original);
            }
            let output = git(&root, &args, GIT_BYTES).await?;
            Ok(ResponseResult::GitDiff {
                text: String::from_utf8_lossy(&output.bytes).into_owned(),
                truncated: output.truncated,
            })
        }),
        Method::GitLog(params) => runtime.block_on(async {
            let root = git_root(&params.cwd).await?;
            // Unborn HEAD is a normal empty history, not a connection failure.
            let mut verify = git_command(&root);
            verify.args(["rev-parse", "--verify", "--quiet", "HEAD"]);
            let head = run_bounded(verify, 1024, GIT_TIMEOUT, true, None).await?;
            if head.bytes.is_empty() {
                let symbolic = git(&root, &["symbolic-ref", "-q", "HEAD"], 8192).await?;
                if !symbolic.bytes.is_empty() {
                    return Ok(ResponseResult::GitLog {
                        root,
                        text: "No commits yet".into(),
                        truncated: false,
                    });
                }
            }
            let output = git(
                &root,
                &[
                    "log",
                    "-30",
                    "--no-show-signature",
                    "--no-decorate",
                    "--format=%h %s",
                    "--",
                ],
                16 * 1024,
            )
            .await?;
            Ok(ResponseResult::GitLog {
                root,
                text: String::from_utf8_lossy(&output.bytes).into_owned(),
                truncated: output.truncated,
            })
        }),
        _ => Err(invalid("not a read method")),
    }
}

use super::read_process::{run_bounded, ProcessOutput as GitOutput};

async fn git_root(cwd: &str) -> io::Result<String> {
    root_path(cwd)?;
    let output = git(cwd, &["rev-parse", "--show-toplevel"], 8192).await?;
    if output.truncated {
        return Err(invalid("Git root too long"));
    }
    let text = String::from_utf8(output.bytes).map_err(|_| invalid("non-UTF8 Git root"))?;
    let root = text.strip_suffix('\n').unwrap_or(&text).to_owned();
    root_path(&root)?;
    Ok(root)
}

fn git_command(cwd: &str) -> std::process::Command {
    let mut command = crate::noninteractive_process::command("git");
    command
        .current_dir(cwd)
        .args([
            "--no-pager",
            "--no-optional-locks",
            "--literal-pathspecs",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "color.ui=false",
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn filter_driver_overrides(bytes: &[u8]) -> io::Result<Vec<String>> {
    if !bytes.is_empty() && !bytes.ends_with(&[0]) {
        return Err(invalid("incomplete Git filter configuration"));
    }
    let text =
        std::str::from_utf8(bytes).map_err(|_| invalid("non-UTF8 Git filter configuration"))?;
    let mut drivers = std::collections::BTreeSet::new();
    for key in text.split('\0').filter(|key| !key.is_empty()) {
        let driver = key
            .strip_prefix("filter.")
            .and_then(|key| {
                [".clean", ".process", ".required"]
                    .iter()
                    .find_map(|suffix| key.strip_suffix(suffix))
            })
            .ok_or_else(|| invalid("invalid Git filter configuration"))?;
        if driver.is_empty()
            || driver.len() > 256
            || !driver
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'_' | b'-' | b'.'))
        {
            return Err(invalid("unsupported Git filter driver name"));
        }
        drivers.insert(driver);
        if drivers.len() > 128 {
            return Err(invalid("too many Git filter drivers"));
        }
    }
    let mut overrides = Vec::new();
    for driver in drivers {
        overrides.extend([
            format!("filter.{driver}.clean="),
            format!("filter.{driver}.process="),
            format!("filter.{driver}.required=false"),
        ]);
    }
    Ok(overrides)
}

async fn git(cwd: &str, args: &[&str], limit: usize) -> io::Result<GitOutput> {
    let mut command = git_command(cwd);
    if args
        .first()
        .is_some_and(|arg| matches!(*arg, "status" | "diff"))
    {
        // Git can otherwise invoke arbitrary clean/process filters even with
        // --no-ext-diff and --no-textconv. Read names only, never config values.
        let mut config = git_command(cwd);
        config.args([
            "config",
            "--null",
            "--name-only",
            "--get-regexp",
            "^filter\\..*\\.(clean|process|required)$",
        ]);
        let keys = run_bounded(config, 16 * 1024, GIT_TIMEOUT, true, None).await?;
        if keys.truncated {
            return Err(invalid("Git filter configuration exceeds read limit"));
        }
        for argument in filter_driver_overrides(&keys.bytes)? {
            command.arg("-c").arg(argument);
        }
    }
    command.args(args);
    run_bounded(command, limit, GIT_TIMEOUT, false, None).await
}

fn parse_status(bytes: &[u8]) -> io::Result<(String, Vec<GitFileEntry>, bool)> {
    let end = bytes
        .iter()
        .rposition(|byte| *byte == 0)
        .map_or(0, |position| position + 1);
    let mut records = bytes[..end].split(|byte| *byte == 0);
    let branch = records
        .next()
        .ok_or_else(|| invalid("missing Git status header"))?;
    if !branch.starts_with(b"## ") {
        return Err(invalid("invalid Git status header"));
    }
    let mut entries = Vec::new();
    let mut incomplete = !bytes.ends_with(&[0]);
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        if record.len() < 4 {
            incomplete = true;
            break;
        }
        let rename = matches!(record[0], b'R' | b'C') || matches!(record[1], b'R' | b'C');
        let original = if rename {
            records.next().filter(|path| !path.is_empty())
        } else {
            None
        };
        if rename && original.is_none() {
            incomplete = true;
            break;
        }
        let Ok(path) = std::str::from_utf8(&record[3..]) else {
            incomplete = true;
            continue;
        };
        let original_path = original.map(std::str::from_utf8).transpose();
        let Ok(original_path) = original_path else {
            incomplete = true;
            continue;
        };
        if entries.len() == LIST_ENTRIES {
            incomplete = true;
            break;
        }
        entries.push(GitFileEntry {
            path: path.to_owned(),
            original_path: original_path.map(str::to_owned),
            index: char::from(record[0]).to_string(),
            worktree: char::from(record[1]).to_string(),
        });
    }
    Ok((
        String::from_utf8_lossy(&branch[3..]).into_owned(),
        entries,
        incomplete,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::{Arc, Barrier};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let serial = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let root =
                std::env::temp_dir().join(format!("herdr-read-{}-{serial}", std::process::id()));
            std::fs::create_dir_all(&root).unwrap();
            Self(root.canonicalize().unwrap())
        }
        fn cwd(&self) -> String {
            self.0.to_str().unwrap().into()
        }
        fn git(&self, args: &[&str]) {
            let output = crate::noninteractive_process::command("git")
                .current_dir(&self.0)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{:?}: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        fn init(&self) {
            self.git(&["init", "--quiet"]);
            self.git(&["config", "user.email", "test@example.invalid"]);
            self.git(&["config", "user.name", "Read Test"]);
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().unwrap()
    }
    fn read(
        root: &Fixture,
        path: &str,
        handle: &tokio::runtime::Handle,
    ) -> io::Result<ResponseResult> {
        execute_read(
            Method::FileRead(FilePathParams {
                root: root.cwd(),
                path: path.into(),
            }),
            handle,
        )
    }

    #[test]
    fn read_paths_reject_absolute_parent_and_relative_roots() {
        for path in ["../outside", "/outside", "a/../../outside"] {
            assert!(relative_path(path).is_err());
        }
        assert!(root_path("relative").is_err());
        assert!(relative_path("literal[abc]\nfile").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn file_reads_confine_symlinks_and_bound_binary_and_listing() {
        let runtime = runtime();
        let root = Fixture::new();
        let outside = Fixture::new();
        std::fs::write(outside.0.join("secret"), "outside").unwrap();
        std::os::unix::fs::symlink(&outside.0, root.0.join("escape")).unwrap();
        std::os::unix::fs::symlink("loop", root.0.join("loop")).unwrap();
        assert!(read(&root, "escape/secret", runtime.handle()).is_err());
        assert!(read(&root, "loop", runtime.handle()).is_err());
        assert!(read(&root, "missing", runtime.handle()).is_err());
        std::fs::write(root.0.join("binary"), [0, 255]).unwrap();
        assert!(
            matches!(read(&root, "binary", runtime.handle()).unwrap(), ResponseResult::FileRead { binary: true, text, .. } if text.is_empty())
        );
        std::fs::write(root.0.join("large"), vec![b'x'; FILE_BYTES + 20]).unwrap();
        assert!(
            matches!(read(&root, "large", runtime.handle()).unwrap(), ResponseResult::FileRead { truncated: true, text, .. } if text.len() == FILE_BYTES)
        );
        std::fs::write(root.0.join("line\n\u{1b}name"), "safe").unwrap();
        for i in 0..LIST_ENTRIES + 5 {
            std::fs::write(root.0.join(format!("entry-{i}")), "").unwrap();
        }
        let result = execute_read(
            Method::FileList(FilePathParams {
                root: root.cwd(),
                path: String::new(),
            }),
            runtime.handle(),
        )
        .unwrap();
        assert!(
            matches!(result, ResponseResult::FileList { entries, truncated: true, .. } if entries.len() <= LIST_ENTRIES)
        );
        assert!(execute_read(
            Method::FileList(FilePathParams {
                root: root.cwd(),
                path: "escape".into()
            }),
            runtime.handle()
        )
        .is_err());
    }

    #[test]
    fn porcelain_status_preserves_dual_changes_rename_conflict_and_control_paths() {
        let bytes =
            b"## main\0MM dual\0R  new name\0old name\0?? a\n\x1b[31m\0 D deleted\0UU conflict\0";
        let (branch, entries, truncated) = parse_status(bytes).unwrap();
        assert_eq!(branch, "main");
        assert!(!truncated);
        assert_eq!(
            (entries[0].index.as_str(), entries[0].worktree.as_str()),
            ("M", "M")
        );
        assert_eq!(entries[1].original_path.as_deref(), Some("old name"));
        assert_eq!(entries[2].path, "a\n\x1b[31m");
        assert_eq!(entries[3].worktree, "D");
        assert_eq!(entries[4].index, "U");
        let (_, entries, truncated) = parse_status(b"## main\0R  incomplete\0").unwrap();
        assert!(entries.is_empty() && truncated);
        let (_, entries, truncated) = parse_status(b"## main\0 M partial").unwrap();
        assert!(entries.is_empty() && truncated);
    }

    #[test]
    fn git_reads_cover_nested_root_unborn_non_git_rename_deleted_and_both_diffs() {
        let runtime = runtime();
        let repo = Fixture::new();
        assert!(execute_read(
            Method::GitStatus(GitReadParams { cwd: repo.cwd() }),
            runtime.handle()
        )
        .is_err());
        repo.init();
        assert!(
            matches!(execute_read(Method::GitLog(GitReadParams { cwd: repo.cwd() }), runtime.handle()).unwrap(), ResponseResult::GitLog { text, .. } if text == "No commits yet")
        );
        std::fs::write(repo.0.join("dual"), "initial\n").unwrap();
        std::fs::write(repo.0.join("old"), "rename\n").unwrap();
        std::fs::write(repo.0.join("deleted"), "delete\n").unwrap();
        repo.git(&["add", "."]);
        repo.git(&["commit", "--quiet", "-m", "initial"]);
        std::fs::write(repo.0.join("dual"), "staged\n").unwrap();
        repo.git(&["add", "dual"]);
        std::fs::write(repo.0.join("dual"), "unstaged\n").unwrap();
        repo.git(&["mv", "old", "new"]);
        std::fs::remove_file(repo.0.join("deleted")).unwrap();
        std::fs::write(repo.0.join("untracked[abc]"), "new file\n").unwrap();
        std::fs::create_dir(repo.0.join("sub")).unwrap();
        let nested = repo.0.join("sub").to_str().unwrap().to_owned();
        let result = execute_read(
            Method::GitStatus(GitReadParams {
                cwd: nested.clone(),
            }),
            runtime.handle(),
        )
        .unwrap();
        let ResponseResult::GitStatus { root, entries, .. } = result else {
            panic!("status");
        };
        assert_eq!(root, repo.cwd());
        assert!(entries
            .iter()
            .any(|entry| entry.path == "dual" && entry.index == "M" && entry.worktree == "M"));
        assert!(entries
            .iter()
            .any(|entry| entry.path == "new" && entry.original_path.as_deref() == Some("old")));
        assert!(entries
            .iter()
            .any(|entry| entry.path == "deleted" && entry.worktree == "D"));
        assert!(entries
            .iter()
            .any(|entry| entry.path == "untracked[abc]" && entry.index == "?"));
        for (staged, expected) in [(true, "+staged"), (false, "+unstaged")] {
            let result = execute_read(
                Method::GitDiff(GitDiffParams {
                    cwd: nested.clone(),
                    path: "dual".into(),
                    original_path: None,
                    staged,
                }),
                runtime.handle(),
            )
            .unwrap();
            assert!(
                matches!(result, ResponseResult::GitDiff { text, .. } if text.contains(expected))
            );
        }
        let result = execute_read(
            Method::GitDiff(GitDiffParams {
                cwd: repo.cwd(),
                path: "new".into(),
                original_path: Some("old".into()),
                staged: true,
            }),
            runtime.handle(),
        )
        .unwrap();
        assert!(
            matches!(result, ResponseResult::GitDiff { text, .. } if text.contains("rename from old"))
        );
        assert!(
            matches!(execute_read(Method::GitLog(GitReadParams { cwd: nested }), runtime.handle()).unwrap(), ResponseResult::GitLog { text, .. } if text.contains("initial"))
        );
        let linked = Fixture::new();
        repo.git(&[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            linked.0.to_str().unwrap(),
        ]);
        let result = execute_read(
            Method::GitStatus(GitReadParams { cwd: linked.cwd() }),
            runtime.handle(),
        )
        .unwrap();
        assert!(matches!(result, ResponseResult::GitStatus { root, .. } if root == linked.cwd()));
    }

    #[cfg(unix)]
    #[test]
    fn git_diff_does_not_read_through_a_replaced_tracked_parent_symlink() {
        let runtime = runtime();
        let repo = Fixture::new();
        let outside = Fixture::new();
        repo.init();
        std::fs::create_dir(repo.0.join("dir")).unwrap();
        std::fs::write(repo.0.join("dir/file"), "tracked\n").unwrap();
        std::fs::write(outside.0.join("file"), "OUTSIDE_CONTENT_SENTINEL\n").unwrap();
        repo.git(&["add", "."]);
        repo.git(&["commit", "--quiet", "-m", "base"]);
        std::fs::remove_dir_all(repo.0.join("dir")).unwrap();
        std::os::unix::fs::symlink(&outside.0, repo.0.join("dir")).unwrap();
        let result = execute_read(
            Method::GitDiff(GitDiffParams {
                cwd: repo.cwd(),
                path: "dir/file".into(),
                original_path: None,
                staged: false,
            }),
            runtime.handle(),
        )
        .unwrap();
        assert!(
            matches!(result, ResponseResult::GitDiff { text, .. } if text.contains("-tracked") && !text.contains("OUTSIDE_CONTENT_SENTINEL"))
        );
    }

    #[test]
    fn git_status_reports_real_merge_conflicts() {
        let runtime = runtime();
        let repo = Fixture::new();
        repo.init();
        repo.git(&["checkout", "--quiet", "-b", "first"]);
        std::fs::write(repo.0.join("conflict"), "base\n").unwrap();
        repo.git(&["add", "."]);
        repo.git(&["commit", "--quiet", "-m", "base"]);
        repo.git(&["checkout", "--quiet", "-b", "second"]);
        std::fs::write(repo.0.join("conflict"), "second\n").unwrap();
        repo.git(&["commit", "--quiet", "-am", "second"]);
        repo.git(&["checkout", "--quiet", "first"]);
        std::fs::write(repo.0.join("conflict"), "first\n").unwrap();
        repo.git(&["commit", "--quiet", "-am", "first"]);
        let merge = crate::noninteractive_process::command("git")
            .current_dir(&repo.0)
            .args(["merge", "--no-edit", "second"])
            .output()
            .unwrap();
        assert!(!merge.status.success());
        let result = execute_read(
            Method::GitStatus(GitReadParams { cwd: repo.cwd() }),
            runtime.handle(),
        )
        .unwrap();
        assert!(
            matches!(result, ResponseResult::GitStatus { entries, .. } if entries.iter().any(|entry| entry.path == "conflict" && entry.index == "U" && entry.worktree == "U"))
        );
    }

    #[test]
    fn git_literal_paths_disable_external_diff_and_cap_output() {
        let runtime = runtime();
        let repo = Fixture::new();
        repo.init();
        std::fs::write(repo.0.join("[x]"), "old\n").unwrap();
        repo.git(&["add", "."]);
        repo.git(&["commit", "--quiet", "-m", "bounded"]);
        repo.git(&["config", "diff.external", "must-not-be-executed"]);
        repo.git(&["config", "core.fsmonitor", "must-not-be-executed"]);
        repo.git(&["config", "filter.unsafe.clean", "must-not-be-executed"]);
        repo.git(&["config", "filter.unsafe.process", "must-not-be-executed"]);
        repo.git(&["config", "filter.unsafe.required", "true"]);
        repo.git(&["config", "diff.unsafe.textconv", "must-not-be-executed"]);
        repo.git(&["config", "diff.unsafe.command", "must-not-be-executed"]);
        std::fs::write(
            repo.0.join(".gitattributes"),
            "* filter=unsafe diff=unsafe\n",
        )
        .unwrap();
        std::fs::write(repo.0.join("[x]"), "changed\n".repeat(GIT_BYTES)).unwrap();
        let result = execute_read(
            Method::GitDiff(GitDiffParams {
                cwd: repo.cwd(),
                path: "[x]".into(),
                original_path: None,
                staged: false,
            }),
            runtime.handle(),
        )
        .unwrap();
        assert!(
            matches!(result, ResponseResult::GitDiff { text, truncated: true } if text.len() <= GIT_BYTES && text.contains("changed"))
        );
        assert!(execute_read(
            Method::GitStatus(GitReadParams { cwd: repo.cwd() }),
            runtime.handle()
        )
        .is_ok());
        let output = runtime
            .block_on(git(&repo.cwd(), &["log", "--format=%h %s"], 4))
            .unwrap();
        assert!(output.truncated && output.bytes.len() == 4);
    }

    #[test]
    fn filter_enumeration_fails_closed_on_invalid_incomplete_or_excessive_keys() {
        assert!(filter_driver_overrides(b"").unwrap().is_empty());
        assert_eq!(
            filter_driver_overrides(b"filter.safe.clean\0filter.safe.process\0")
                .unwrap()
                .len(),
            3
        );
        for bytes in [
            b"filter.bad name.clean\0".as_slice(),
            b"filter.safe.clean",
            b"filter..clean\0",
            b"unrelated.clean\0",
            b"filter.\xff.clean\0",
        ] {
            assert!(filter_driver_overrides(bytes).is_err());
        }
        let many = (0..129)
            .map(|i| format!("filter.driver{i}.clean\0"))
            .collect::<String>();
        assert!(filter_driver_overrides(many.as_bytes()).is_err());
        let runtime = runtime();
        let repo = Fixture::new();
        repo.init();
        // A name the safe override parser cannot reliably represent must not
        // lead to an unprotected diff/status invocation.
        repo.git(&["config", "filter.bad name.clean", "must-not-be-executed"]);
        assert!(execute_read(
            Method::GitStatus(GitReadParams { cwd: repo.cwd() }),
            runtime.handle()
        )
        .is_err());
    }

    #[test]
    fn bounded_deferred_worker_does_not_block_ping_and_busy_does_not_spawn() {
        let runtime = runtime();
        let _guard = runtime.enter();
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let release = Arc::new(Barrier::new(2));
        let barrier = release.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        spawn_read_worker(slots.clone(), "slow".into(), tx.clone(), move || {
            barrier.wait();
            Ok(ResponseResult::Ok {})
        });
        spawn_read_worker(slots.clone(), "busy".into(), tx, || {
            panic!("must not spawn when saturated")
        });
        let busy = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(busy.contains("read_busy"));
        let (api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let pong = crate::api::handle_front_door_for_test(
            Request {
                id: "ping".into(),
                method: Method::Ping(PingParams::default()),
            },
            &api_tx,
        );
        let pong: SuccessResponse = serde_json::from_str(&pong).unwrap();
        assert_eq!(pong.id, "ping");
        assert!(matches!(pong.result, ResponseResult::Pong { .. }));
        assert!(
            api_rx.is_empty(),
            "ping must not wait on the blocked read lane"
        );
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        let response = app.handle_api_request_after_internal_events_drained(Request {
            id: "runtime-list".into(),
            method: Method::WorkspaceList(EmptyParams::default()),
        });
        let response: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(response.id, "runtime-list");
        assert!(
            matches!(response.result, ResponseResult::WorkspaceList { workspaces } if workspaces.is_empty())
        );
        assert_eq!(
            slots.available_permits(),
            0,
            "slow read must still hold its slot"
        );
        release.wait();
        assert!(rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .contains("slow"));
        assert_eq!(
            slots.available_permits(),
            1,
            "completed read must return its slot"
        );
    }

    // Invoked only by the timeout test as an isolated child, never by normal test runs.
    #[test]
    #[ignore]
    fn bounded_read_child_fixture() {
        let Some(path) = std::env::var_os("HERDR_READ_CHILD_PID") else {
            return;
        };
        std::fs::write(path, std::process::id().to_string()).unwrap();
        std::thread::sleep(Duration::from_secs(60));
    }

    #[test]
    fn timed_out_subprocess_is_killed_and_reaped() {
        let runtime = runtime();
        let fixture = Fixture::new();
        let pid_path = fixture.0.join("pid");
        let mut command = crate::noninteractive_process::command(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "app::api::reads::tests::bounded_read_child_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("HERDR_READ_CHILD_PID", &pid_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let result = runtime.block_on(run_bounded(
            command,
            1024,
            Duration::from_secs(2),
            false,
            None,
        ));
        assert!(matches!(result, Err(ref error) if error.kind() == io::ErrorKind::TimedOut));
        let pid: u32 = std::fs::read_to_string(pid_path).unwrap().parse().unwrap();
        assert!(
            !crate::platform::process_exists(pid),
            "child must no longer exist (including as a zombie)"
        );
    }
}
