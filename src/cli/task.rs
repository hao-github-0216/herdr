use crate::api::schema::*;
use clap::ArgMatches;
use std::io::{self, Read};

pub(super) fn run_task_command(args: &[String]) -> io::Result<i32> {
    let mut root = super::spec::command();
    let Some(command) = root.find_subcommand_mut("task") else {
        return Err(io::Error::other("task CLI specification missing"));
    };
    let matches = match command
        .clone()
        .try_get_matches_from(std::iter::once("task".to_owned()).chain(args.iter().cloned()))
    {
        Ok(matches) => matches,
        Err(error) => {
            if error.use_stderr() {
                eprintln!("{error}");
            } else {
                print!("{error}");
            }
            return Ok(error.exit_code());
        }
    };
    if let Some(("adapter", args)) = matches.subcommand() {
        return super::task_adapter::run(args);
    }
    let method = match parse_method(&matches) {
        Ok(method) => method,
        Err(error) => {
            eprintln!("{error}\nSee: herdr task <command> --help (includes examples)");
            return Ok(2);
        }
    };
    let response = super::send_request(&Request {
        id: "cli:task".into(),
        method,
    })?;
    super::print_response(&response)
}
fn value(matches: &ArgMatches, key: &str) -> Option<String> {
    matches.get_one::<String>(key).cloned()
}
fn required(matches: &ArgMatches, key: &str) -> io::Result<String> {
    value(matches, key)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("missing {key}")))
}
fn provenance(matches: &ArgMatches) -> TaskProvenance {
    TaskProvenance {
        reporter: value(matches, "reporter"),
        agent: value(matches, "agent"),
        boot_id: value(matches, "source-boot"),
        pane_id: value(matches, "source-pane"),
        agent_session: value(matches, "agent-session"),
    }
}
fn enum_value<T: serde::de::DeserializeOwned>(
    matches: &ArgMatches,
    key: &str,
) -> io::Result<Option<T>> {
    value(matches, key)
        .map(|value| {
            serde_json::from_value(serde_json::Value::String(value))
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
        })
        .transpose()
}
fn read_text(matches: &ArgMatches, name: &str, max: usize) -> io::Result<Option<String>> {
    let (file, stdin) = if name == "text" {
        ("text-file", "text-stdin")
    } else {
        ("description-file", "description-stdin")
    };
    let value = if let Some(value) = value(matches, name) {
        Some(value)
    } else if let Some(path) = value(matches, file) {
        Some(read_bounded(std::fs::File::open(path)?, max)?)
    } else if matches.get_flag(stdin) {
        Some(read_bounded(io::stdin().lock(), max)?)
    } else {
        None
    };
    if value.as_ref().is_some_and(|value| value.len() > max) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} exceeds {max} bytes"),
        ));
    }
    Ok(value)
}
fn read_bounded(reader: impl Read, max: usize) -> io::Result<String> {
    let mut bytes = Vec::new();
    reader.take(max as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("text input exceeds {max} bytes"),
        ));
    }
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}
fn parse_method(matches: &ArgMatches) -> io::Result<Method> {
    let (name, args) = matches
        .subcommand()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "task command required"))?;
    Ok(match name {
        "create" => Method::TaskCreate(TaskCreateParams {
            nonce: required(args, "nonce")?,
            title: required(args, "title")?,
            description: read_text(args, "description", 8192)?,
            status: enum_value(args, "status")?,
            workspace_id: value(args, "workspace"),
            workspace_label: value(args, "workspace-label"),
            provenance: provenance(args),
        }),
        "source" => Method::TaskSourceContext(TaskSourceContextParams {
            pane_id: required(args, "pane")?,
        }),
        "list" => Method::TaskList(TaskListParams {
            workspace_id: value(args, "workspace"),
            status: value(args, "status"),
            after: value(args, "after"),
            limit: args.get_one::<usize>("limit").copied(),
        }),
        "get" => Method::TaskGet(TaskTarget {
            task_id: required(args, "task_id")?,
        }),
        "update" => Method::TaskUpdate(TaskUpdateParams {
            task_id: required(args, "task_id")?,
            expected_revision: args
                .get_one::<u64>("expected-revision")
                .copied()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "expected revision required")
                })?,
            title: value(args, "title"),
            description: read_text(args, "description", 8192)?,
            status: enum_value(args, "status")?,
            reported_verification: enum_value(args, "reported-verification")?,
            verification_evidence: value(args, "verification-evidence"),
            workspace_id: value(args, "workspace"),
            workspace_label: value(args, "workspace-label"),
        }),
        "add-conclusion" => Method::TaskAppendConclusion(TaskAppendConclusionParams {
            task_id: required(args, "task_id")?,
            expected_revision: args
                .get_one::<u64>("expected-revision")
                .copied()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "expected revision required")
                })?,
            nonce: required(args, "nonce")?,
            text: read_text(args, "text", 4096)?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "conclusion text required")
            })?,
            provenance: provenance(args),
        }),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unknown task command",
            ))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(args: &[&str]) -> Method {
        let mut command = super::super::spec::command();
        let matches = command
            .find_subcommand_mut("task")
            .unwrap()
            .clone()
            .try_get_matches_from(std::iter::once("task").chain(args.iter().copied()))
            .unwrap();
        parse_method(&matches).unwrap()
    }
    #[test]
    fn task_cli_builds_explicit_cas_and_nonce_without_pane_input() {
        assert!(
            matches!(parse(&["create", "--title", "one", "--nonce", "stable", "--json"]), Method::TaskCreate(TaskCreateParams { nonce, .. }) if nonce == "stable")
        );
        assert!(
            matches!(parse(&["add-conclusion", "task_1", "--expected-revision", "2", "--nonce", "stable", "--text", "line one\nline two"]), Method::TaskAppendConclusion(TaskAppendConclusionParams { expected_revision: 2, text, .. }) if text.contains('\n'))
        );
        assert!(matches!(
            parse(&[
                "update",
                "task_1",
                "--expected-revision",
                "3",
                "--status",
                "done"
            ]),
            Method::TaskUpdate(TaskUpdateParams {
                reported_verification: None,
                ..
            })
        ));
        assert!(matches!(
            parse(&["list", "--limit", "5", "--after", "task_1"]),
            Method::TaskList(TaskListParams { limit: Some(5), .. })
        ));
        assert!(matches!(parse(&["get", "task_1"]), Method::TaskGet(_)));
        assert!(
            matches!(parse(&["source", "--pane", "pane_1", "--json"]), Method::TaskSourceContext(TaskSourceContextParams { pane_id }) if pane_id == "pane_1")
        );
    }
    #[test]
    fn task_cli_missing_flags_fail_before_any_stdin_or_network() {
        for args in [
            vec!["task", "source", "--json"],
            vec!["task", "create", "--title", "x", "--description-stdin"],
            vec!["task", "update", "task_1", "--status", "done"],
            vec!["task", "add-conclusion", "task_1", "--text-stdin"],
            vec!["task", "update", "task_1", "--expected-revision", "1"],
            vec![
                "task", "create", "--title", "x", "--nonce", "n", "--status", "pending",
            ],
        ] {
            let mut root = super::super::spec::command();
            assert!(root
                .find_subcommand_mut("task")
                .unwrap()
                .clone()
                .try_get_matches_from(args)
                .is_err());
        }
    }
    #[test]
    fn task_cli_every_help_has_examples() {
        let mut root = super::super::spec::command();
        let command = root.find_subcommand_mut("task").unwrap();
        assert!(command.render_long_help().to_string().contains("Examples:"));
        for command in command.get_subcommands_mut() {
            assert!(command.render_long_help().to_string().contains("Examples:"));
        }
    }
    #[test]
    fn task_cli_text_reader_is_bounded_and_multiline() {
        assert_eq!(read_bounded("a\nb".as_bytes(), 3).unwrap(), "a\nb");
        assert!(read_bounded("abcd".as_bytes(), 3).is_err());
        assert!(read_bounded([255].as_slice(), 3).is_err());
    }
}
