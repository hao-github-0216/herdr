//! Phase-1 Tasks service/CLI integration. Every process gets a synthetic HOME,
//! configuration, working directory and endpoint; never contacts an installed server.
#![cfg(unix)]
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Sandbox {
    root: PathBuf,
}
impl Sandbox {
    fn new() -> Self {
        let root = PathBuf::from(format!(
            "/tmp/htask-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        for directory in ["config", "state", "data", "cache", "runtime", "work"] {
            fs::create_dir(root.join(directory)).unwrap();
        }
        // Debug/release builds have separate configuration namespace spellings.
        for app in ["herdr", "herdr-dev"] {
            fs::create_dir(root.join("config").join(app)).unwrap();
            fs::write(
                root.join("config").join(app).join("config.toml"),
                "onboarding = false\n",
            )
            .unwrap();
        }
        Self { root }
    }
    fn command(&self, socket: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .env("HERDR_SOCKET_PATH", socket)
            .env(
                "HERDR_CLIENT_SOCKET_PATH",
                socket.with_extension("client.sock"),
            )
            .env("SHELL", "/bin/sh")
            .env("TERM", "xterm-256color")
            .current_dir(self.root.join("work"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }
    fn spawn(&self, name: &str, fail_import: bool) -> Server {
        let socket = self.root.join("runtime").join(format!("{name}.sock"));
        let mut command = self.command(&socket);
        command.arg("server");
        if fail_import {
            command.env("HERDR_TEST_HANDOFF_IMPORT_FAIL", "after_restored");
        }
        let child = command.spawn().unwrap();
        let mut server = Server { child, socket };
        server.wait_ready();
        server
    }
    fn cli(&self, socket: &Path, args: &[&str], stdin: Option<&str>) -> Value {
        let mut command = self.command(socket);
        command
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if stdin.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut child = command.spawn().unwrap();
        if let Some(text) = stdin {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(text.as_bytes())
                .unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "CLI {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn store_paths(&self) -> Vec<PathBuf> {
        fn walk(path: &Path, result: &mut Vec<PathBuf>) {
            for entry in fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, result);
                } else if path.file_name().is_some_and(|name| name == "store.json") {
                    result.push(path);
                }
            }
        }
        let mut result = Vec::new();
        walk(&self.root.join("config"), &mut result);
        result
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
struct Server {
    child: Child,
    socket: PathBuf,
}
impl Server {
    fn wait_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            if UnixStream::connect(&self.socket).is_ok() {
                return;
            }
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "synthetic server exited"
            );
            assert!(Instant::now() < deadline, "synthetic socket not ready");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn request(&self, method: &str, params: Value) -> Value {
        request(&self.socket, method, params)
    }
    fn stop(&mut self) {
        if UnixStream::connect(&self.socket).is_ok() {
            let _ = self.request("server.stop", json!({}));
        }
        let deadline = Instant::now() + Duration::from_secs(4);
        while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        // A successful handoff has a replacement child not owned by this handle.
        // server.stop above addresses only this sandbox endpoint; wait for socket removal.
        let deadline = Instant::now() + Duration::from_secs(4);
        while self.socket.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}
fn request(socket: &Path, method: &str, params: Value) -> Value {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    writeln!(
        stream,
        "{}",
        json!({"id":"tasks-test","method":method,"params":params})
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap_or_else(|error| panic!("{method}: {error}; {line:?}"))
}
fn ok(response: &Value) {
    assert!(response.get("error").is_none(), "{response}");
}
fn terminal_identity(server: &Server, pane: &Value) -> (Value, u64) {
    let record = server.request("pane.get", json!({"pane_id":pane}));
    ok(&record);
    let process = server.request("pane.process_info", json!({"pane_id":pane}));
    ok(&process);
    let shell_pid = process["result"]["process_info"]["shell_pid"]
        .as_u64()
        .unwrap();
    assert_eq!(unsafe { libc::kill(shell_pid as libc::pid_t, 0) }, 0);
    assert_eq!(&record["result"]["pane"]["pane_id"], pane);
    assert!(record["result"]["pane"]["terminal_id"]
        .as_str()
        .is_some_and(|id| id.starts_with("term_") && id.len() > 5));
    // Existing restore allocates a new TerminalId after importing the same PTY
    // (persist/restore.rs:637-648,689-690,729-730). Pane + live shell PID, not
    // terminal-runtime ID continuity, characterizes the preexisting contract.
    (record["result"]["pane"]["pane_id"].clone(), shell_pid)
}

fn create(server: &Server, nonce: &str) -> Value {
    let response = server.request(
        "task.create",
        json!({"title":"Synthetic task","nonce":nonce}),
    );
    ok(&response);
    response
}

#[test]
fn tasks_cli_persists_restart_and_workspace_closure_with_explicit_reported_verification() {
    let sandbox = Sandbox::new();
    let mut server = sandbox.spawn("one", false);
    let workspace = server.request(
        "workspace.create",
        json!({"cwd":sandbox.root.join("work"), "focus":false}),
    );
    ok(&workspace);
    let workspace_id = workspace["result"]["workspace"]["workspace_id"]
        .as_str()
        .unwrap();
    let created = sandbox.cli(
        &server.socket,
        &[
            "task",
            "create",
            "--title",
            "Synthetic CLI task",
            "--nonce",
            "create-one",
            "--workspace",
            workspace_id,
            "--description-stdin",
            "--json",
        ],
        Some("first line\nsecond line"),
    );
    let id = created["result"]["task"]["task_id"].as_str().unwrap();
    let store_id = created["result"]["store_id"].clone();
    let updated = sandbox.cli(
        &server.socket,
        &[
            "task",
            "update",
            id,
            "--expected-revision",
            "1",
            "--status",
            "done",
            "--json",
        ],
        None,
    );
    assert_eq!(
        updated["result"]["task"]["reported_verification"],
        "unknown"
    );
    fs::write(
        sandbox.root.join("conclusion.txt"),
        "saved conclusion\nreported checks",
    )
    .unwrap();
    let concluded = sandbox.cli(
        &server.socket,
        &[
            "task",
            "add-conclusion",
            id,
            "--expected-revision",
            "2",
            "--nonce",
            "append-one",
            "--text-file",
            sandbox.root.join("conclusion.txt").to_str().unwrap(),
            "--json",
        ],
        None,
    );
    ok(&server.request("workspace.close", json!({"workspace_id":workspace_id})));
    server.stop();
    drop(server);
    let restarted = sandbox.spawn("one", false);
    let saved = sandbox.cli(&restarted.socket, &["task", "get", id, "--json"], None);
    assert_eq!(saved["result"]["task"], concluded["result"]["task"]);
    assert_eq!(saved["result"]["store_id"], store_id);
    let replay = sandbox.cli(
        &restarted.socket,
        &[
            "task",
            "add-conclusion",
            id,
            "--expected-revision",
            "2",
            "--nonce",
            "append-one",
            "--text-file",
            sandbox.root.join("conclusion.txt").to_str().unwrap(),
            "--json",
        ],
        None,
    );
    assert_eq!(replay["result"]["replayed"], true);
    let list = sandbox.cli(
        &restarted.socket,
        &["task", "list", "--workspace", workspace_id, "--json"],
        None,
    );
    assert_eq!(list["result"]["tasks"].as_array().unwrap().len(), 1);
}
#[test]
fn tasks_api_endpoint_isolation_nonce_replay_and_concurrent_cas() {
    let sandbox = Sandbox::new();
    let first = sandbox.spawn("first", false);
    let second = sandbox.spawn("second", false);
    let created = create(&first, "one");
    let other = create(&second, "one");
    assert_ne!(created["result"]["store_id"], other["result"]["store_id"]);
    let id = created["result"]["task"]["task_id"].as_str().unwrap();
    assert_eq!(
        second.request("task.get", json!({"task_id":id}))["error"]["code"],
        "task_not_found"
    );
    assert_eq!(create(&first, "one")["result"]["replayed"], true);
    let workers = (0..2)
        .map(|_| {
            let socket = first.socket.clone();
            let id = id.to_owned();
            std::thread::spawn(move || {
                request(
                    &socket,
                    "task.update",
                    json!({"task_id":id,"expected_revision":1,"status":"working"}),
                )
            })
        })
        .collect::<Vec<_>>();
    let results = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        results
            .iter()
            .filter(|result| result.get("error").is_none())
            .count(),
        1
    );
    assert!(results
        .iter()
        .any(|result| result["error"]["code"] == "task_revision_conflict"));
    assert_eq!(sandbox.store_paths().len(), 2);
}
#[test]
fn tasks_corruption_disables_only_tasks_ordinary_terminal_and_json_api_remain_usable() {
    let sandbox = Sandbox::new();
    let server = sandbox.spawn("one", false);
    create(&server, "one");
    let path = sandbox.store_paths().pop().unwrap();
    for bytes in [
        b"corrupt".to_vec(),
        br#"{"version":99,"store_id":"future","tasks":{},"nonces":{}}"#.to_vec(),
    ] {
        fs::write(&path, &bytes).unwrap();
        assert_eq!(
            server.request("task.list", json!({}))["error"]["code"],
            "task_unavailable"
        );
        ok(&server.request("ping", json!({})));
        let workspace = server.request(
            "workspace.create",
            json!({"cwd":sandbox.root.join("work"),"focus":false}),
        );
        ok(&workspace);
        let pane = &workspace["result"]["root_pane"]["pane_id"];
        assert_eq!(
            server.request("task.source_context", json!({"pane_id":pane}))["error"]["code"],
            "task_unavailable"
        );
        ok(&server.request(
            "pane.read",
            json!({"pane_id":pane,"format":"text","source":"visible"}),
        ));
        ok(&server.request(
            "workspace.close",
            json!({"workspace_id":workspace["result"]["workspace"]["workspace_id"]}),
        ));
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
}
#[test]
fn tasks_pending_write_live_handoff_preserves_ack_nonce_and_terminal_identity() {
    let sandbox = Sandbox::new();
    let server = sandbox.spawn("handoff", false);
    let workspace = server.request(
        "workspace.create",
        json!({"cwd":sandbox.root.join("work"),"focus":false}),
    );
    ok(&workspace);
    let pane = workspace["result"]["root_pane"]["pane_id"].clone();
    let terminal_before = terminal_identity(&server, &pane);
    let created = create(&server, "one");
    let id = created["result"]["task"]["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let store_id = created["result"]["store_id"].clone();
    let path = sandbox.store_paths().pop().unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.parent().unwrap().join("lock"))
        .unwrap();
    lock.lock().unwrap();
    let socket = server.socket.clone();
    let append_params = json!({"task_id":id,"expected_revision":1,"nonce":"append","text":"pending before handoff"});
    let params = append_params.clone();
    let write = std::thread::spawn(move || request(&socket, "task.append_conclusion", params));
    // Service-level tests prove deterministic admission/drain ordering. Here the
    // OS lock holds the real request open while an actual import overlaps.
    std::thread::sleep(Duration::from_millis(100));
    let socket = server.socket.clone();
    let handoff = std::thread::spawn(move || request(&socket, "server.live_handoff", json!({})));
    std::thread::sleep(Duration::from_millis(100));
    drop(lock);
    let acknowledged = write.join().unwrap();
    ok(&acknowledged);
    ok(&handoff.join().unwrap());
    let saved = server.request("task.get", json!({"task_id":id}));
    assert_eq!(saved["result"]["task"], acknowledged["result"]["task"]);
    assert_eq!(saved["result"]["store_id"], store_id);
    assert_eq!(
        server.request("task.append_conclusion", append_params)["result"]["replayed"],
        true
    );
    assert_eq!(terminal_identity(&server, &pane), terminal_before);
    ok(&server.request(
        "pane.read",
        json!({"pane_id":pane,"format":"text","source":"visible"}),
    ));
}
#[test]
fn tasks_import_failure_rolls_back_without_losing_task_or_terminal() {
    let sandbox = Sandbox::new();
    let server = sandbox.spawn("rollback", true);
    let workspace = server.request(
        "workspace.create",
        json!({"cwd":sandbox.root.join("work"),"focus":false}),
    );
    ok(&workspace);
    let pane = workspace["result"]["root_pane"]["pane_id"].clone();
    let terminal_before = terminal_identity(&server, &pane);
    let created = create(&server, "one");
    let id = &created["result"]["task"]["task_id"];
    let append = json!({"task_id":id,"expected_revision":1,"nonce":"append","text":"acknowledged before failed import"});
    let acknowledged = server.request("task.append_conclusion", append.clone());
    ok(&acknowledged);
    let failure = server.request("server.live_handoff", json!({}));
    assert_eq!(failure["error"]["code"], "handoff_failed");
    assert!(
        server.request("task.append_conclusion", append)["result"]["replayed"]
            .as_bool()
            .unwrap()
    );
    assert_eq!(
        server.request("task.get", json!({"task_id":id}))["result"]["task"],
        acknowledged["result"]["task"]
    );
    ok(&server.request(
        "task.update",
        json!({"task_id":id,"expected_revision":2,"status":"review"}),
    ));
    assert_eq!(terminal_identity(&server, &pane), terminal_before);
    ok(&server.request(
        "pane.read",
        json!({"pane_id":pane,"format":"text","source":"visible"}),
    ));
}

#[test]
fn tasks_public_source_context_cli_create_append_focus_and_restart_proof() {
    let sandbox = Sandbox::new();
    let mut server = sandbox.spawn("source-workflow", false);
    let workspace = server.request(
        "workspace.create",
        json!({"cwd":sandbox.root.join("work"),"focus":false}),
    );
    ok(&workspace);
    let pane = workspace["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap();
    let observed = sandbox.cli(
        &server.socket,
        &["task", "source", "--pane", pane, "--json"],
        None,
    );
    ok(&observed);
    assert_eq!(observed["result"]["type"], "task_source_context");
    let context = &observed["result"]["context"];
    assert_eq!(context["pane_id"], pane);
    assert_eq!(
        context["workspace_id"],
        workspace["result"]["workspace"]["workspace_id"]
    );
    assert!(context["agent"].is_null());
    let boot = context["boot_id"].as_str().unwrap();
    assert!(!boot.is_empty());
    let api_context = server.request("task.source_context", json!({"pane_id":pane}));
    assert_eq!(api_context["result"], observed["result"]);
    assert_eq!(
        server.request("task.source_context", json!({"pane_id":"not-a-pane"}))["error"]["code"],
        "pane_not_found"
    );
    assert!(server
        .request(
            "task.source_context",
            json!({"pane_id":pane,"boot_id":"forged"})
        )
        .get("error")
        .is_some());
    assert!(server
        .request(
            "task.source_context",
            json!({"pane_id":pane,"store_path":"/not-allowed"})
        )
        .get("error")
        .is_some());
    let created = sandbox.cli(
        &server.socket,
        &[
            "task",
            "create",
            "--title",
            "Public source workflow",
            "--nonce",
            "source-create",
            "--source-boot",
            boot,
            "--source-pane",
            context["pane_id"].as_str().unwrap(),
            "--json",
        ],
        None,
    );
    let id = created["result"]["task"]["task_id"].as_str().unwrap();
    assert_eq!(created["result"]["store_id"], context["store_id"]);
    let appended = sandbox.cli(
        &server.socket,
        &[
            "task",
            "add-conclusion",
            id,
            "--expected-revision",
            "1",
            "--nonce",
            "source-append",
            "--text",
            "Durable public workflow conclusion",
            "--source-boot",
            boot,
            "--source-pane",
            pane,
            "--json",
        ],
        None,
    );
    ok(&appended);
    let saved_source = &appended["result"]["task"]["conclusions"][0]["source"];
    assert_eq!(saved_source["store_id"], context["store_id"]);
    assert_eq!(saved_source["reported"]["boot_id"], context["boot_id"]);
    assert_eq!(saved_source["reported"]["pane_id"], context["pane_id"]);
    assert!(saved_source["reported"]["agent_session"].is_null());
    let before_list = server.request("task.list", json!({}));
    ok(&before_list);
    // Existing public focus method accepts the actual resolved pane before restart.
    let focused = server.request("pane.focus", json!({"pane_id":context["pane_id"]}));
    ok(&focused);
    assert_eq!(focused["result"]["pane"]["pane_id"], context["pane_id"]);
    server.stop();
    drop(server);
    let restarted = sandbox.spawn("source-workflow", false);
    let retained = sandbox.cli(&restarted.socket, &["task", "get", id, "--json"], None);
    assert_eq!(retained["result"]["task"], appended["result"]["task"]);
    let current = restarted.request("task.source_context", json!({"pane_id":pane}));
    ok(&current);
    assert_eq!(
        current["result"]["context"]["store_id"],
        context["store_id"]
    );
    assert_ne!(current["result"]["context"]["boot_id"], context["boot_id"]);
    let after_list = restarted.request("task.list", json!({}));
    ok(&after_list);
    // Optional external proof feeds the ignored client guard test with actual
    // public runtime responses, never a hand-injected boot ID. No repository writes.
    if let Some(path) = std::env::var_os("HERDR_TASK_WORKFLOW_PROOF") {
        fs::write(
            path,
            serde_json::to_vec_pretty(&json!({
                "observed":observed,"focused":focused,"appended":appended,
                "current":current,"retained":retained,"before_list":before_list,"after_list":after_list
            }))
            .unwrap(),
        )
        .unwrap();
    }
}

fn adapter(
    sandbox: &Sandbox,
    socket: &Path,
    pane: &str,
    workspace: &str,
    method: Option<&str>,
    input: &str,
) -> (bool, Value) {
    let mut command = sandbox.command(socket);
    command
        .env("HERDR_BIN_PATH", env!("CARGO_BIN_EXE_herdr"))
        .env("HERDR_PANE_ID", pane)
        .env("HERDR_WORKSPACE_ID", workspace)
        .args(["task", "adapter"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(method) = method {
        command.args(["call", method]);
    } else {
        command.arg("discover");
    }
    let mut child = command.spawn().unwrap();
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.as_bytes());
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.stderr.is_empty(),
        "adapter stdout is machine-only; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "adapter JSON {e}: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    (output.status.success(), value)
}

fn fixture_context_fingerprint(
    socket: &Path,
    store: &str,
    boot: &str,
    pane: &str,
    workspace: &str,
) -> String {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(&json!([
        "herdr:tasks-adapter-context",
        1,
        socket.to_str().unwrap(),
        store,
        boot,
        pane,
        workspace
    ]))
    .unwrap();
    format!("hctx1:{:x}", Sha256::digest(bytes))
}

#[test]
fn tasks_bound_adapter_real_cli_cas_nonce_replay_scope_and_restart_stale_context() {
    let sandbox = Sandbox::new();
    let mut server = sandbox.spawn("adapter-real", false);
    let created = server.request(
        "workspace.create",
        json!({"cwd":sandbox.root.join("work"),"focus":false}),
    );
    ok(&created);
    let pane = created["result"]["root_pane"]["pane_id"].as_str().unwrap();
    let workspace = created["result"]["workspace"]["workspace_id"]
        .as_str()
        .unwrap();
    let (success, discover) = adapter(&sandbox, &server.socket, pane, workspace, None, "");
    assert!(success, "{discover}");
    assert_eq!(discover["result"]["availability"], "probed");
    assert!(server.request("task.list", json!({}))["result"]["tasks"]
        .as_array()
        .unwrap()
        .is_empty());
    let guard = &discover["context_fingerprint"];
    let (success, source) = adapter(
        &sandbox,
        &server.socket,
        pane,
        workspace,
        Some("source_context"),
        "{}",
    );
    assert!(success, "{source}");
    assert_eq!(&source["context_fingerprint"], guard);
    assert!(source["result"]["context"]
        .get("context_fingerprint")
        .is_none());
    let payload = json!({"title":"Explicit adapter report 漢字 👩‍💻", "nonce":"adapter-retained", "expected_context":guard}).to_string();
    let (success, first) = adapter(
        &sandbox,
        &server.socket,
        pane,
        workspace,
        Some("create"),
        &payload,
    );
    assert!(success, "{first}");
    let task_id = first["result"]["task"]["task_id"].as_str().unwrap();
    assert_eq!(first["result"]["task"]["workspace_id"], workspace);
    assert_eq!(
        first["result"]["task"]["source"]["reported"]["pane_id"],
        pane
    );
    let (success, replay) = adapter(
        &sandbox,
        &server.socket,
        pane,
        workspace,
        Some("create"),
        &payload,
    );
    assert!(success, "{replay}");
    assert_eq!(replay["result"]["replayed"], true);
    assert_eq!(replay["result"]["task"]["task_id"], task_id);
    let update =
        json!({"task_id":task_id,"expected_revision":1,"status":"review","expected_context":guard})
            .to_string();
    assert!(
        adapter(
            &sandbox,
            &server.socket,
            pane,
            workspace,
            Some("update"),
            &update
        )
        .0
    );
    let (success, conflict) = adapter(
        &sandbox,
        &server.socket,
        pane,
        workspace,
        Some("update"),
        &update,
    );
    assert!(!success);
    assert_eq!(conflict["error"]["code"], "task_revision_conflict");
    let append = json!({"task_id":task_id,"expected_revision":2,"nonce":"adapter-conclusion","text":"Saved report, not acceptance","expected_context":guard}).to_string();
    let (success, conclusion) = adapter(
        &sandbox,
        &server.socket,
        pane,
        workspace,
        Some("append_conclusion"),
        &append,
    );
    assert!(success, "{conclusion}");
    let (success, replayed) = adapter(
        &sandbox,
        &server.socket,
        pane,
        workspace,
        Some("append_conclusion"),
        &append,
    );
    assert!(success, "{replayed}");
    assert_eq!(replayed["result"]["replayed"], true);
    assert_eq!(
        replayed["result"]["conclusion_id"],
        conclusion["result"]["conclusion_id"]
    );
    let (success, current) = adapter(
        &sandbox,
        &server.socket,
        pane,
        workspace,
        Some("list"),
        r#"{"scope":"current"}"#,
    );
    assert!(success, "{current}");
    assert_eq!(current["result"]["tasks"].as_array().unwrap().len(), 1);
    ok(&server.request(
        "task.create",
        json!({"nonce":"unassigned","title":"Orphan"}),
    ));
    let (success, all) = adapter(
        &sandbox,
        &server.socket,
        pane,
        workspace,
        Some("list"),
        r#"{"scope":"all"}"#,
    );
    assert!(success, "{all}");
    assert_eq!(all["result"]["tasks"].as_array().unwrap().len(), 2);
    // Simulate a lost create reply: keep the caller's exact operation/nonce.
    // The caller-retained guard fails closed after restart, even with an existing
    // durable nonce. It does not re-enrich the payload or infer the prior outcome.
    server.stop();
    drop(server);
    let restarted = sandbox.spawn("adapter-real", false);
    let (success, stale) = adapter(
        &sandbox,
        &restarted.socket,
        pane,
        workspace,
        Some("create"),
        &payload,
    );
    assert!(!success, "{stale}");
    assert_eq!(stale["error"]["code"], "stale_context");
    assert_eq!(
        restarted.request("task.list", json!({}))["result"]["tasks"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn tasks_bound_adapter_uncertain_no_ledger_or_committed_restart_and_binding_drift_never_dispatch_retry(
) {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    };
    for (change, committed) in [
        ("boot", false),
        ("boot", true),
        ("store", false),
        ("pane", false),
        ("workspace", false),
        ("socket", false),
        ("guard", false),
    ] {
        let sandbox = Sandbox::new();
        let socket = sandbox.root.join("runtime/retention.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let context = Arc::new(Mutex::new(
            json!({"store_id":"store_fixture","boot_id":"boot_fixture","pane_id":"pane_fixture","workspace_id":"workspace_fixture","agent":null}),
        ));
        let calls = Arc::new(Mutex::new(Vec::<Value>::new()));
        // Synthetic durable-ledger stand-in: a lost response may follow either
        // no commit/no nonce record or a commit. The retry must not distinguish them.
        let ledger = Arc::new(Mutex::new(Vec::<Value>::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (observed, recorded, saved, shutdown) =
            (context.clone(), calls.clone(), ledger.clone(), stop.clone());
        let worker = std::thread::spawn(move || {
            while !shutdown.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("synthetic listener: {error}"),
                };
                // The listener is nonblocking only to make shutdown bounded.
                // Request reads need blocking mode; a timeout does not clear it.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(4)))
                    .unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                recorded.lock().unwrap().push(request.clone());
                let context = observed.lock().unwrap().clone();
                match request["method"].as_str().unwrap() {
                    "task.source_context" => writeln!(stream,"{}",json!({"id":request["id"],"result":{"type":"task_source_context","context":context}})).unwrap(),
                    "task.list" => writeln!(stream,"{}",json!({"id":request["id"],"result":{"type":"task_list","store_id":context["store_id"],"tasks":[],"next_after":null}})).unwrap(),
                    _ => {
                        if committed { saved.lock().unwrap().push(request); }
                        // Drop the response after receiving the mutation. No automatic retries.
                    }
                }
            }
        });
        let (success, discovery) = adapter(
            &sandbox,
            &socket,
            "pane_fixture",
            "workspace_fixture",
            None,
            "",
        );
        assert!(success, "{discovery}");
        let guard = discovery["context_fingerprint"].as_str().unwrap();
        let original = json!({"title":"uncertain report","nonce":"never-replace-this-nonce","expected_context":guard}).to_string();
        let (success, uncertain) = adapter(
            &sandbox,
            &socket,
            "pane_fixture",
            "workspace_fixture",
            Some("create"),
            &original,
        );
        assert!(!success);
        assert_eq!(uncertain["error"]["code"], "response_uncertain");
        let before = calls.lock().unwrap().len();
        let mut changed_socket = socket.clone();
        let (mut pane, mut workspace) = ("pane_fixture", "workspace_fixture");
        let mut retained_guard = guard.to_string();
        match change {
            "boot" => context.lock().unwrap()["boot_id"] = json!("boot_restarted"),
            "store" => context.lock().unwrap()["store_id"] = json!("store_replaced"),
            "pane" => {
                pane = "pane_changed";
                context.lock().unwrap()["pane_id"] = json!(pane);
            }
            "workspace" => {
                workspace = "workspace_changed";
                context.lock().unwrap()["workspace_id"] = json!(workspace);
            }
            "socket" => changed_socket = sandbox.root.join("runtime/./retention.sock"),
            "guard" => retained_guard = format!("hctx1:{}", "0".repeat(64)),
            _ => unreachable!(),
        }
        for name in ["create", "update", "append_conclusion"] {
            let input = match name {
                "create" if change != "guard" => original.clone(), // byte-identical caller JSON after restart
                "create" => json!({"title":"uncertain report","nonce":"never-replace-this-nonce","expected_context":retained_guard}).to_string(),
                "update" => json!({"task_id":"task_fixture","expected_revision":1,"status":"review","expected_context":retained_guard}).to_string(),
                _ => json!({"task_id":"task_fixture","expected_revision":1,"nonce":"retained-conclusion","text":"saved report","expected_context":retained_guard}).to_string(),
            };
            let (success, stale) = adapter(
                &sandbox,
                &changed_socket,
                pane,
                workspace,
                Some(name),
                &input,
            );
            assert!(!success, "{change}: {stale}");
            assert_eq!(stale["error"]["code"], "stale_context", "{change}: {stale}");
            assert!(stale["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Previous outcome is unknown"));
        }
        stop.store(true, Ordering::SeqCst);
        worker.join().unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(before, 5); // discovery probes, then two preflight reads and one uncertain create
        assert_eq!(calls.len(), before + 3); // each retry observes only context; zero mutations
        assert!(calls[before..]
            .iter()
            .all(|r| r["method"] == "task.source_context"));
        assert_eq!(calls[4]["method"], "task.create");
        assert_eq!(calls[4]["params"]["nonce"], "never-replace-this-nonce");
        assert_eq!(calls[4]["params"]["provenance"]["boot_id"], "boot_fixture");
        assert!(calls[4]["params"].get("expected_context").is_none());
        assert_eq!(ledger.lock().unwrap().len(), usize::from(committed));
    }
}

#[test]
fn tasks_bound_adapter_missing_invalid_binding_and_reserved_args_never_probe() {
    let sandbox = Sandbox::new();
    let socket = sandbox.root.join("runtime/no-probe.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let output = sandbox
        .command(&socket)
        .args(["task", "adapter", "discover"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(!output.status.success());
    assert_eq!(value["error"]["code"], "unavailable");
    assert!(listener.accept().is_err());
    for input in [
        r#"{"scope":"all","socket":"/different"}"#,
        r#"{"scope":"current","pane_id":"other"}"#,
        r#"{"scope":"all","provenance":{}}"#,
        r#"{"scope":"all","limit":"1"}"#,
        r#"{"scope":"all","status":"\u001b"}"#,
    ] {
        let (success, error) = adapter(
            &sandbox,
            &socket,
            "pane_fixture",
            "workspace_fixture",
            Some("list"),
            input,
        );
        assert!(!success);
        assert_eq!(error["error"]["code"], "invalid_arguments");
        assert!(listener.accept().is_err());
    }
    let guard = format!("hctx1:{}", "a".repeat(64));
    for name in ["create", "update", "append_conclusion"] {
        let arguments = match name {
            "create" => json!({"title":"fixture","nonce":"retained"}),
            "update" => json!({"task_id":"task_fixture","expected_revision":1,"status":"review"}),
            _ => {
                json!({"task_id":"task_fixture","expected_revision":1,"nonce":"retained","text":"report"})
            }
        };
        for malformed in [
            Value::Null,
            json!(1),
            json!(""),
            json!("hctx2:bad"),
            json!(format!("hctx1:{}", "A".repeat(64))),
            json!(format!("{guard}\n")),
            json!(format!("{guard}x")),
        ] {
            let mut input = arguments.clone();
            input["expected_context"] = malformed;
            let (success, error) = adapter(
                &sandbox,
                &socket,
                "pane_fixture",
                "workspace_fixture",
                Some(name),
                &input.to_string(),
            );
            assert!(!success);
            assert_eq!(error["error"]["code"], "invalid_arguments");
            assert!(listener.accept().is_err());
        }
        let (success, error) = adapter(
            &sandbox,
            &socket,
            "pane_fixture",
            "workspace_fixture",
            Some(name),
            &arguments.to_string(),
        );
        assert!(!success);
        assert_eq!(error["error"]["code"], "invalid_arguments");
        assert!(listener.accept().is_err());
        for reserved in [
            "socket",
            "binary",
            "pane_id",
            "workspace_id",
            "boot_id",
            "provenance",
        ] {
            let mut input = arguments.clone();
            input["expected_context"] = json!(guard);
            input[reserved] = json!("other");
            let (success, error) = adapter(
                &sandbox,
                &socket,
                "pane_fixture",
                "workspace_fixture",
                Some(name),
                &input.to_string(),
            );
            assert!(!success);
            assert_eq!(error["error"]["code"], "invalid_arguments");
            assert!(listener.accept().is_err());
        }
        // A valid guard never postpones validation of the remaining arguments.
        let input = json!({"expected_context":guard});
        let (success, error) = adapter(
            &sandbox,
            &socket,
            "pane_fixture",
            "workspace_fixture",
            Some(name),
            &input.to_string(),
        );
        assert!(!success);
        assert_eq!(error["error"]["code"], "invalid_arguments");
        assert!(listener.accept().is_err());
    }
    let (success, error) = adapter(
        &sandbox,
        &socket,
        "pane_fixture",
        "workspace_fixture",
        Some("list"),
        &" ".repeat(16385),
    );
    assert!(!success);
    assert_eq!(error["error"]["code"], "invalid_arguments");
    assert!(listener.accept().is_err());
}

#[test]
fn tasks_bound_adapter_socket_faults_are_bounded_and_post_send_uncertain() {
    use std::sync::{Arc, Mutex};
    for scenario in [
        "wrong-id",
        "malformed",
        "unknown-field",
        "wrong-type",
        "unknown-success",
        "malformed-error",
        "wrong-store",
        "wrong-pane",
        "domain-unsupported",
        "uncertain",
        "huge",
        "disconnect",
        "timeout",
    ] {
        let sandbox = Sandbox::new();
        let socket = sandbox.root.join("runtime/fixture.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let worker = std::thread::spawn(move || {
            for step in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                recorded.lock().unwrap().push(request.clone());
                if step == 0 && scenario == "domain-unsupported" {
                    writeln!(stream,"{}",json!({"id":request["id"],"error":{"code":"unknown_method","message":"old server"}})).unwrap();
                    break;
                }
                if step == 0 && scenario == "wrong-pane" {
                    writeln!(stream,"{}",json!({"id":request["id"],"result":{"type":"task_source_context","context":{"store_id":"store_fixture","boot_id":"boot_fixture","pane_id":"other","workspace_id":"workspace_fixture","agent":null}}})).unwrap();
                    break;
                }
                if step == 0 {
                    writeln!(stream,"{}",json!({"id":request["id"],"result":{"type":"task_source_context","context":{"store_id":"store_fixture","boot_id":"boot_fixture","pane_id":"pane_fixture","workspace_id":"workspace_fixture","agent":null}}})).unwrap();
                    continue;
                }
                if step == 1 {
                    writeln!(stream,"{}",json!({"id":request["id"],"result":{"type":"task_list","store_id":if scenario=="wrong-store" {"other"}else{"store_fixture"},"tasks":[],"next_after":null}})).unwrap();
                    if scenario == "wrong-store" {
                        break;
                    }
                    continue;
                }
                match scenario {
                    "wrong-id" => {
                        let _ = writeln!(
                            stream,
                            "{}",
                            json!({"id":"other","result":{"type":"task_list","store_id":"store_fixture","tasks":[],"next_after":null}})
                        );
                    }
                    "malformed" => {
                        let _ = writeln!(stream, "{{not-json}}");
                    }
                    "unknown-field" => {
                        let _ = writeln!(
                            stream,
                            "{}",
                            json!({"id":request["id"],"error":{"code":"fake","message":"fake","reserved":true}})
                        );
                    }
                    "unknown-success" => {
                        let _ = writeln!(
                            stream,
                            "{}",
                            json!({"id":request["id"],"result":{"type":"task_list","store_id":"store_fixture","tasks":[],"next_after":null,"reserved":true}})
                        );
                    }
                    "malformed-error" => {
                        let _ = writeln!(
                            stream,
                            "{}",
                            json!({"id":request["id"],"error":{"code":9,"message":false}})
                        );
                    }
                    "wrong-type" => {
                        let _ = writeln!(
                            stream,
                            "{}",
                            json!({"id":request["id"],"result":{"type":"unknown_result"}})
                        );
                    }
                    "uncertain" => {
                        let _ = writeln!(
                            stream,
                            "{}",
                            json!({"id":request["id"],"error":{"code":"task_commit_uncertain","message":"durability unknown"}})
                        );
                    }
                    "huge" => {
                        let _ = writeln!(stream, "{}", "x".repeat(262145));
                    }
                    "disconnect" => {}
                    "timeout" => std::thread::sleep(Duration::from_millis(3300)),
                    _ => unreachable!(),
                }
            }
        });
        let start = Instant::now();
        let (success, error) = adapter(
            &sandbox,
            &socket,
            "pane_fixture",
            "workspace_fixture",
            Some("create"),
            &json!({"title":"fixture","nonce":"retained","expected_context":fixture_context_fingerprint(&socket,"store_fixture","boot_fixture","pane_fixture","workspace_fixture")}).to_string(),
        );
        assert!(!success, "{scenario}: {error}");
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "{scenario} exceeded bound"
        );
        let expected = match scenario {
            "wrong-store" | "wrong-pane" => "unavailable",
            "domain-unsupported" => "unknown_method",
            "uncertain" => "task_commit_uncertain",
            "timeout" => "commit_uncertain",
            _ => "response_uncertain",
        };
        assert_eq!(error["error"]["code"], expected, "{scenario}: {error}");
        worker.join().unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls[0]["method"], "task.source_context");
        if calls.len() > 1 {
            assert_eq!(calls[1]["method"], "task.list");
        }
        assert!(calls.len() <= 3); // No automatic retry, including on uncertainty.
    }
}

#[test]
fn tasks_bound_adapter_stalled_stdin_and_disconnected_endpoint_have_no_fallback_or_writes() {
    let sandbox = Sandbox::new();
    let socket = sandbox.root.join("runtime/no-input.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut command = sandbox.command(&socket);
    command
        .env("HERDR_BIN_PATH", env!("CARGO_BIN_EXE_herdr"))
        .env("HERDR_PANE_ID", "pane_fixture")
        .env("HERDR_WORKSPACE_ID", "workspace_fixture")
        .args(["task", "adapter", "call", "create"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let held_stdin = child.stdin.take().unwrap();
    let before = Instant::now();
    let output = child.wait_with_output().unwrap();
    drop(held_stdin);
    assert!(before.elapsed() < Duration::from_secs(4));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["error"]["code"], "commit_uncertain");
    assert!(listener.accept().is_err());
    let (success, result) = adapter(
        &sandbox,
        &socket.with_extension("missing"),
        "pane_fixture",
        "workspace_fixture",
        None,
        "",
    );
    assert!(!success);
    assert_eq!(result["error"]["code"], "unavailable");
    assert!(listener.accept().is_err());
    let output = sandbox
        .command(&socket)
        .env("HERDR_BIN_PATH", "/nonexistent/foreign-herdr")
        .env("HERDR_PANE_ID", "pane_fixture")
        .env("HERDR_WORKSPACE_ID", "workspace_fixture")
        .args(["task", "adapter", "discover"])
        .stdout(Stdio::piped())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["error"]["code"], "unavailable");
    assert!(listener.accept().is_err());
}
