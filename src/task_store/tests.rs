use super::*;
use std::sync::{Arc, Barrier};

struct Fixture {
    root: PathBuf,
    store: TaskStore,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(new_id("herdr-task-test"));
        std::fs::create_dir_all(&root).unwrap();
        let store = TaskStore::for_endpoint(&root.join("config"), &root.join("api.sock")).unwrap();
        Self { root, store }
    }
    fn restart(&self) -> TaskStore {
        TaskStore::for_endpoint(&self.root.join("config"), &self.root.join("api.sock")).unwrap()
    }
    fn path(&self) -> PathBuf {
        self.store.directory.join("store.json")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn create(nonce: &str) -> Method {
    Method::TaskCreate(TaskCreateParams {
        nonce: nonce.into(),
        title: "Implement tasks".into(),
        workspace_id: Some("closed_workspace".into()),
        workspace_label: Some("Preserved workspace".into()),
        provenance: TaskProvenance {
            boot_id: Some("old_boot".into()),
            pane_id: Some("pane_1".into()),
            ..Default::default()
        },
        ..Default::default()
    })
}
fn task(result: ResponseResult) -> TaskInfo {
    match result {
        ResponseResult::TaskRecord { task, .. } => task,
        _ => panic!("expected task"),
    }
}
fn get(store: &TaskStore, id: &str) -> TaskInfo {
    task(
        store
            .execute(Method::TaskGet(TaskTarget { task_id: id.into() }))
            .unwrap(),
    )
}
fn append(id: &str, revision: u64, nonce: &str) -> Method {
    Method::TaskAppendConclusion(TaskAppendConclusionParams { task_id: id.into(), expected_revision: revision, nonce: nonce.into(), text: "Conclusion survives scrollback and pane closure\nchecks reported, not independently verified".into(), provenance: TaskProvenance::default() })
}
fn update(id: &str, revision: u64) -> Method {
    Method::TaskUpdate(TaskUpdateParams {
        task_id: id.into(),
        expected_revision: revision,
        status: Some(TaskStatus::Done),
        ..Default::default()
    })
}

#[test]
fn task_store_restart_preserves_cas_conclusions_and_closed_source_metadata() {
    let fixture = Fixture::new();
    let created = task(fixture.store.execute(create("create-one")).unwrap());
    let updated = task(
        fixture
            .restart()
            .execute(update(&created.task_id, 1))
            .unwrap(),
    );
    assert_eq!(updated.status, "done");
    assert_eq!(updated.reported_verification, "unknown");
    let concluded = task(
        fixture
            .restart()
            .execute(append(&created.task_id, 2, "conclusion-one"))
            .unwrap(),
    );
    assert_eq!(concluded.revision, 3);
    assert_eq!(get(&fixture.restart(), &created.task_id), concluded);
    assert_eq!(concluded.source.reported.pane_id.as_deref(), Some("pane_1"));
    assert_eq!(
        concluded.workspace_label.as_deref(),
        Some("Preserved workspace")
    );
    assert_eq!(
        concluded.conclusions[0].source.store_id,
        concluded.source.store_id
    );
    assert!(concluded.conclusions[0]
        .conclusion_id
        .starts_with("conclusion_"));
}
#[test]
fn task_store_nonce_replays_restart_without_duplicates_and_rejects_mismatch() {
    let fixture = Fixture::new();
    let created = task(fixture.store.execute(create("one")).unwrap());
    let response = fixture.restart().execute(create("one")).unwrap();
    assert!(matches!(
        response,
        ResponseResult::TaskRecord { replayed: true, .. }
    ));
    assert_eq!(task(response), created);
    let mut changed = create("one");
    if let Method::TaskCreate(ref mut params) = changed {
        params.title = "different".into();
    }
    assert_eq!(
        fixture.store.execute(changed).unwrap_err().code,
        "task_nonce_mismatch"
    );
    let method = append(&created.task_id, 1, "append-one");
    let first = fixture.store.execute(method.clone()).unwrap();
    let second = fixture.restart().execute(method.clone()).unwrap();
    assert!(matches!(
        &second,
        ResponseResult::TaskRecord { replayed: true, .. }
    ));
    assert_eq!(task(first), task(second));
    assert_eq!(get(&fixture.store, &created.task_id).conclusions.len(), 1);
    let mut changed = method;
    if let Method::TaskAppendConclusion(ref mut params) = changed {
        params.expected_revision = 2;
    }
    assert_eq!(
        fixture.store.execute(changed).unwrap_err().code,
        "task_nonce_mismatch"
    );
    assert_eq!(fixture.store.load().unwrap().tasks.len(), 1);
}
#[test]
fn task_store_concurrent_cas_has_exactly_one_winner() {
    let fixture = Fixture::new();
    let created = task(fixture.store.execute(create("one")).unwrap());
    let barrier = Arc::new(Barrier::new(3));
    let workers = (0..2)
        .map(|_| {
            let store = fixture.restart();
            let barrier = barrier.clone();
            let method = update(&created.task_id, 1);
            std::thread::spawn(move || {
                barrier.wait();
                store.execute(method)
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let results = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .into_iter()
            .find_map(|result| result.err())
            .unwrap()
            .code,
        "task_revision_conflict"
    );
    assert_eq!(get(&fixture.store, &created.task_id).revision, 2);
}
#[test]
fn task_store_named_and_socket_namespaces_are_isolated_and_boot_independent() {
    let fixture = Fixture::new();
    let other_socket = TaskStore::for_endpoint(
        &fixture.root.join("config"),
        &fixture.root.join("other.sock"),
    )
    .unwrap();
    let named = TaskStore::for_endpoint(
        &fixture.root.join("config/sessions/named"),
        &fixture.root.join("api.sock"),
    )
    .unwrap();
    let created = task(fixture.store.execute(create("one")).unwrap());
    for store in [other_socket, named] {
        assert_ne!(store.directory, fixture.store.directory);
        assert_eq!(
            store
                .execute(Method::TaskGet(TaskTarget {
                    task_id: created.task_id.clone()
                }))
                .unwrap_err()
                .code,
            "task_not_found"
        );
        let independent = task(store.execute(create("one")).unwrap());
        assert_ne!(independent.source.store_id, created.source.store_id);
        assert_eq!(
            independent.source.reported.pane_id,
            created.source.reported.pane_id
        );
    }
    assert_eq!(fixture.store.directory, fixture.restart().directory);
}
#[test]
fn task_store_corrupt_future_oversize_are_unchanged_not_overwritten() {
    let fixture = Fixture::new();
    fixture.store.execute(create("one")).unwrap();
    let original = std::fs::read(fixture.path()).unwrap();
    let mut future: serde_json::Value = serde_json::from_slice(&original).unwrap();
    future["version"] = 99.into();
    for bytes in [
        b"not-json".to_vec(),
        serde_json::to_vec(&future).unwrap(),
        vec![b' '; MAX_STORE_BYTES + 1],
    ] {
        std::fs::write(fixture.path(), &bytes).unwrap();
        assert_eq!(
            fixture.store.execute(create("new")).unwrap_err().code,
            "task_unavailable"
        );
        assert_eq!(std::fs::read(fixture.path()).unwrap(), bytes);
    }
}
#[test]
fn task_store_faults_preserve_commit_or_report_uncertainty_resolved_by_nonce() {
    let fixture = Fixture::new();
    let created = task(fixture.store.execute(create("one")).unwrap());
    let committed = std::fs::read(fixture.path()).unwrap();
    for fault in [Fault::BeforeWrite, Fault::BeforeReplace] {
        assert_eq!(
            fixture
                .store
                .execute_with_fault(append(&created.task_id, 1, "append"), fault)
                .unwrap_err()
                .code,
            "task_unavailable"
        );
        assert_eq!(std::fs::read(fixture.path()).unwrap(), committed);
    }
    assert_eq!(
        fixture
            .store
            .execute_with_fault(append(&created.task_id, 1, "append"), Fault::AfterReplace)
            .unwrap_err()
            .code,
        "task_commit_uncertain"
    );
    let resolved = fixture
        .restart()
        .execute(append(&created.task_id, 1, "append"))
        .unwrap();
    assert!(matches!(
        &resolved,
        ResponseResult::TaskRecord { replayed: true, .. }
    ));
    assert_eq!(task(resolved).conclusions.len(), 1);
    assert_eq!(
        fixture
            .store
            .execute_with_fault(update(&created.task_id, 2), Fault::AfterReplace)
            .unwrap_err()
            .code,
        "task_commit_uncertain"
    );
    assert_eq!(get(&fixture.restart(), &created.task_id).revision, 3);
    assert_eq!(
        fixture
            .store
            .execute(update(&created.task_id, 2))
            .unwrap_err()
            .code,
        "task_revision_conflict"
    );
}
#[test]
fn task_store_list_is_bounded_paginated_and_does_not_silently_truncate() {
    let fixture = Fixture::new();
    for n in 0..7 {
        fixture
            .store
            .execute(create(&format!("create-{n}")))
            .unwrap();
    }
    let mut after = None;
    let mut ids = std::collections::BTreeSet::new();
    loop {
        let ResponseResult::TaskList {
            tasks, next_after, ..
        } = fixture
            .store
            .execute(Method::TaskList(TaskListParams {
                after,
                limit: Some(2),
                ..Default::default()
            }))
            .unwrap()
        else {
            panic!()
        };
        assert!(tasks.len() <= 2);
        for task in tasks {
            assert!(ids.insert(task.task_id));
        }
        after = next_after;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(ids.len(), 7);
    assert_eq!(
        fixture
            .store
            .execute(Method::TaskList(TaskListParams {
                limit: Some(101),
                ..Default::default()
            }))
            .unwrap_err()
            .code,
        "invalid_request"
    );
}
#[test]
fn task_store_strict_writes_bounds_and_append_capacity_preserve_last_commit() {
    let fixture = Fixture::new();
    assert!(serde_json::from_value::<Request>(serde_json::json!({"id":"1","method":"task.create","params":{"nonce":"one","title":"x","status":"pending"}})).is_err());
    assert!(serde_json::from_value::<Request>(serde_json::json!({"id":"1","method":"task.create","params":{"nonce":"one","title":"x","path":"/arbitrary"}})).is_err());
    let created = task(fixture.store.execute(create("one")).unwrap());
    for n in 0..16 {
        fixture
            .store
            .execute(append(&created.task_id, n + 1, &format!("append-{n}")))
            .unwrap();
    }
    let bytes = std::fs::read(fixture.path()).unwrap();
    assert_eq!(
        fixture
            .store
            .execute(append(&created.task_id, 17, "append-17"))
            .unwrap_err()
            .code,
        "invalid_request"
    );
    assert_eq!(std::fs::read(fixture.path()).unwrap(), bytes);
    let mut huge = create("huge");
    if let Method::TaskCreate(ref mut p) = huge {
        p.description = Some("x".repeat(8193));
    }
    assert_eq!(
        fixture.store.execute(huge).unwrap_err().code,
        "invalid_request"
    );
    assert_eq!(std::fs::read(fixture.path()).unwrap(), bytes);
}
#[test]
fn task_store_read_tolerates_future_status_without_claiming_its_meaning() {
    let fixture = Fixture::new();
    let created = task(fixture.store.execute(create("one")).unwrap());
    let mut board = fixture.store.load().unwrap();
    board.tasks.get_mut(&created.task_id).unwrap().status = "future_status".into();
    fixture.store.save(&board, Fault::None).unwrap();
    assert_eq!(
        get(&fixture.restart(), &created.task_id).status,
        "future_status"
    );
}
#[test]
fn task_store_private_permissions_and_nonregular_store_fail_closed() {
    let fixture = Fixture::new();
    fixture.store.execute(create("one")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&fixture.store.directory)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(fixture.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    std::fs::remove_file(fixture.path()).unwrap();
    std::fs::create_dir(fixture.path()).unwrap();
    assert_eq!(
        fixture.store.execute(create("two")).unwrap_err().code,
        "task_unavailable"
    );
}

// Spawn a second copy of this synthetic unit test process to prove OS lock
// behavior across overlapping old/import processes, not just thread exclusion.
#[test]
fn task_store_cross_process_lock_child() {
    let Some(root) = std::env::var_os("HERDR_TASK_TEST_CHILD_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let store = TaskStore::for_endpoint(&root.join("config"), &root.join("api.sock")).unwrap();
    std::fs::write(root.join("child-started"), b"ready").unwrap();
    let result = store.execute(create("child-create"));
    assert!(result.is_ok(), "{result:?}");
    std::fs::write(root.join("child-done"), b"committed").unwrap();
}
#[test]
fn task_store_overlapping_process_lock_is_transaction_scoped_without_handoff_deadlock() {
    let fixture = Fixture::new();
    let lock = fixture.store.lock().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("task_store::tests::task_store_cross_process_lock_child")
        .arg("--nocapture")
        .env("HERDR_TASK_TEST_CHILD_ROOT", &fixture.root)
        .env("HOME", &fixture.root)
        .env("XDG_CONFIG_HOME", fixture.root.join("config"))
        .env("XDG_DATA_HOME", fixture.root.join("data"))
        .env("XDG_STATE_HOME", fixture.root.join("state"))
        .env("XDG_RUNTIME_DIR", fixture.root.join("runtime"))
        .env_remove("HERDR_SOCKET_PATH")
        .env_remove("HERDR_CLIENT_SOCKET_PATH")
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !fixture.root.join("child-started").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(fixture.root.join("child-started").exists());
    assert!(!fixture.root.join("child-done").exists());
    drop(lock);
    let status = child.wait().unwrap();
    assert!(status.success());
    assert!(fixture.root.join("child-done").exists());
    let task = task(fixture.restart().execute(create("child-create")).unwrap());
    assert_eq!(fixture.store.load().unwrap().tasks.len(), 1);
    assert_eq!(task.revision, 1);
}

#[test]
fn task_store_ambiguous_create_resolves_stable_ids_and_nonce_after_restart() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture
            .store
            .execute_with_fault(create("uncertain-create"), Fault::AfterReplace)
            .unwrap_err()
            .code,
        "task_commit_uncertain"
    );
    let saved = fixture.store.load().unwrap();
    let response = fixture
        .restart()
        .execute(create("uncertain-create"))
        .unwrap();
    assert!(matches!(
        &response,
        ResponseResult::TaskRecord { replayed: true, .. }
    ));
    assert_eq!(task(response).task_id, *saved.tasks.keys().next().unwrap());
    assert_eq!(fixture.store.load().unwrap().store_id, saved.store_id);
    assert_eq!(fixture.store.load().unwrap().tasks.len(), 1);
}

#[test]
fn task_store_task_nonce_and_total_byte_capacity_never_evict_or_change_last_commit() {
    let fixture = Fixture::new();
    let created = task(fixture.store.execute(create("one")).unwrap());
    let mut board = fixture.store.load().unwrap();
    for n in 0..MAX_TASKS {
        let mut task = created.clone();
        task.task_id = format!("task_{n:064x}");
        board.tasks.insert(task.task_id.clone(), task);
    }
    board.tasks.remove(&created.task_id);
    board.nonces.clear();
    fixture.store.save(&board, Fault::None).unwrap();
    let committed = std::fs::read(fixture.path()).unwrap();
    assert_eq!(
        fixture
            .store
            .execute(create("over-task-limit"))
            .unwrap_err()
            .code,
        "task_capacity"
    );
    assert_eq!(std::fs::read(fixture.path()).unwrap(), committed);

    board.tasks.clear();
    board.tasks.insert(created.task_id.clone(), created.clone());
    for n in 0..MAX_NONCES {
        board.nonces.insert(
            format!("create:nonce-{n}"),
            Nonce {
                fingerprint: "a".repeat(64),
                task_id: created.task_id.clone(),
                conclusion_id: None,
            },
        );
    }
    fixture.store.save(&board, Fault::None).unwrap();
    let committed = std::fs::read(fixture.path()).unwrap();
    assert_eq!(
        fixture
            .store
            .execute(create("over-nonce-limit"))
            .unwrap_err()
            .code,
        "task_capacity"
    );
    assert_eq!(std::fs::read(fixture.path()).unwrap(), committed);

    board.nonces.clear();
    board.tasks.clear();
    for n in 0..MAX_TASKS {
        let mut task = created.clone();
        task.task_id = format!("task_{n:064x}");
        task.description = Some("d".repeat(8192));
        task.conclusions = (0..16)
            .map(|i| TaskConclusion {
                conclusion_id: format!("conclusion_{i:064x}"),
                text: "c".repeat(4096),
                recorded_at_unix_ms: 1,
                source: task.source.clone(),
            })
            .collect();
        validate_task(&task, &board.store_id).unwrap();
        board.tasks.insert(task.task_id.clone(), task);
    }
    assert_eq!(
        fixture.store.save(&board, Fault::None).unwrap_err().code,
        "task_capacity"
    );
    assert_eq!(std::fs::read(fixture.path()).unwrap(), committed);
}

#[test]
fn task_store_maximum_list_response_and_task_response_fit_endpoint_limits() {
    let fixture = Fixture::new();
    let created = task(fixture.store.execute(create("one")).unwrap());
    let mut board = fixture.store.load().unwrap();
    board.nonces.clear();
    board.tasks.clear();
    for n in 0..100 {
        let mut task = created.clone();
        task.task_id = format!("task_{n:064x}");
        task.title = "\u{001b}".repeat(512);
        task.workspace_label = Some("\u{001b}".repeat(512));
        task.workspace_id = Some("\u{001b}".repeat(256));
        task.status = "\u{001b}".repeat(64);
        task.reported_verification = "\u{001b}".repeat(64);
        validate_task(&task, &board.store_id).unwrap();
        board.tasks.insert(task.task_id.clone(), task);
    }
    fixture.store.save(&board, Fault::None).unwrap();
    let result = fixture
        .store
        .execute(Method::TaskList(TaskListParams {
            limit: Some(100),
            ..Default::default()
        }))
        .unwrap();
    let bytes = serde_json::to_vec(&SuccessResponse {
        id: "r".repeat(128),
        result,
    })
    .unwrap();
    assert!(
        bytes.len() < crate::server::client_commands::MAX_ENDPOINT_COMMAND_BYTES,
        "{}",
        bytes.len()
    );
    let mut task = created;
    task.conclusions = (0..16)
        .map(|n| TaskConclusion {
            conclusion_id: format!("conclusion_{n:064x}"),
            text: "\u{001b}".repeat(4096),
            recorded_at_unix_ms: 1,
            source: task.source.clone(),
        })
        .collect();
    assert_eq!(
        validate_task(&task, &board.store_id).unwrap_err().code,
        "invalid_request"
    );
}

#[cfg(unix)]
#[test]
fn task_store_bound_symlink_parent_endpoints_have_independent_boards_and_nonces() {
    use std::os::unix::{
        fs::symlink,
        net::{UnixListener, UnixStream},
    };
    // macOS sockaddr_un has a small SUN_LEN; use only a short synthetic /tmp path.
    let root = PathBuf::from("/tmp").join(&new_id("hts")[..24]);
    std::fs::create_dir(&root).unwrap();
    let store = TaskStore::for_endpoint(&root.join("config"), &root.join("api.sock")).unwrap();
    let fixture = Fixture { root, store };
    std::fs::create_dir_all(fixture.root.join("other/child")).unwrap();
    symlink(fixture.root.join("other/child"), fixture.root.join("link")).unwrap();
    let first_path = fixture.root.join("link/../api.sock");
    let second_path = fixture.root.join("api.sock");
    let first_listener = UnixListener::bind(&first_path).unwrap();
    let second_listener = UnixListener::bind(&second_path).unwrap();
    // Both really bind and accept at different filesystem endpoints.
    let _first_client = UnixStream::connect(&first_path).unwrap();
    let _second_client = UnixStream::connect(&second_path).unwrap();
    let _first_peer = first_listener.accept().unwrap();
    let _second_peer = second_listener.accept().unwrap();
    let namespace = fixture.root.join("config");
    let first = TaskStore::for_endpoint(&namespace, &first_path).unwrap();
    let second = TaskStore::for_endpoint(&namespace, &second_path).unwrap();
    assert_ne!(first.directory, second.directory);
    let a = task(first.execute(create("shared-nonce")).unwrap());
    let mut other_payload = create("shared-nonce");
    if let Method::TaskCreate(ref mut params) = other_payload {
        params.title = "independent payload".into();
    }
    let b = task(second.execute(other_payload.clone()).unwrap());
    assert_ne!(a.source.store_id, b.source.store_id);
    assert_ne!(a.task_id, b.task_id);
    assert!(matches!(
        second.execute(other_payload).unwrap(),
        ResponseResult::TaskRecord { replayed: true, .. }
    ));
    assert!(matches!(
        first.execute(create("shared-nonce")).unwrap(),
        ResponseResult::TaskRecord { replayed: true, .. }
    ));
    assert_eq!(
        first
            .execute(Method::TaskGet(TaskTarget { task_id: b.task_id }))
            .unwrap_err()
            .code,
        "task_not_found"
    );
    // Parent aliases share identity, and replacing the socket at handoff does not change it.
    let canonical =
        TaskStore::for_endpoint(&namespace, &fixture.root.join("other/api.sock")).unwrap();
    assert_eq!(canonical.directory, first.directory);
    drop(first_listener);
    std::fs::remove_file(&first_path).unwrap();
    let _replacement = UnixListener::bind(&first_path).unwrap();
    assert_eq!(
        first.directory,
        TaskStore::for_endpoint(&namespace, &first_path)
            .unwrap()
            .directory
    );
    // Apply the same semantic resolution to configuration/session namespaces.
    let nested =
        TaskStore::for_endpoint(&fixture.root.join("link/../config"), &second_path).unwrap();
    assert_ne!(nested.directory, second.directory);
    assert_eq!(
        nested.directory,
        TaskStore::for_endpoint(&fixture.root.join("other/config"), &second_path)
            .unwrap()
            .directory
    );
}

#[cfg(unix)]
#[test]
fn task_store_unresolvable_namespace_or_endpoint_rejects_without_storage() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    symlink("missing", fixture.root.join("dangling")).unwrap();
    for namespace in [
        fixture.root.join("dangling"),
        fixture.root.join("absent/../config"),
    ] {
        assert!(TaskStore::for_endpoint(&namespace, &fixture.root.join("api.sock")).is_err());
    }
    assert!(TaskStore::for_endpoint(
        &fixture.root.join("config"),
        &fixture.root.join("dangling/api.sock")
    )
    .is_err());
    assert!(!fixture.root.join("config/tasks-v1").exists());
}
