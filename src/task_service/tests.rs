use super::*;
use crate::api::schema::*;

fn fixture() -> (std::path::PathBuf, TaskStore, TaskService) {
    let root = std::env::temp_dir().join(format!(
        "herdr-task-service-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let store = TaskStore::for_endpoint(&root.join("config"), &root.join("api.sock")).unwrap();
    let service = TaskService::new(Ok(store.clone()));
    (root, store, service)
}
fn request() -> Request {
    Request {
        id: "create".into(),
        method: Method::TaskCreate(TaskCreateParams {
            nonce: "stable".into(),
            title: "durable".into(),
            ..Default::default()
        }),
    }
}
fn receive(rx: std::sync::mpsc::Receiver<String>) -> serde_json::Value {
    serde_json::from_str(&rx.recv_timeout(Duration::from_secs(5)).unwrap()).unwrap()
}

#[test]
fn task_service_quiesce_drains_acknowledged_write_before_handoff_and_rollback_reopens() {
    let (root, store, service) = fixture();
    let (tx, rx) = std::sync::mpsc::channel();
    service.start(request(), tx);
    service.quiesce(Duration::from_secs(5)).unwrap();
    assert_eq!(receive(rx)["result"]["task"]["revision"], 1);
    let (tx, rx) = std::sync::mpsc::channel();
    service.start(request(), tx);
    assert_eq!(receive(rx)["error"]["code"], "task_quiescing");
    let replacement = TaskService::new(Ok(store));
    let (tx, rx) = std::sync::mpsc::channel();
    replacement.start(request(), tx);
    let replay = receive(rx);
    assert_eq!(replay["result"]["replayed"], true);
    assert_eq!(replay["result"]["task"]["revision"], 1);
    service.resume();
    let (tx, rx) = std::sync::mpsc::channel();
    service.start(request(), tx);
    assert_eq!(
        receive(rx)["result"]["task"]["task_id"],
        replay["result"]["task"]["task_id"]
    );
    service.quiesce(Duration::from_secs(5)).unwrap();
    replacement.quiesce(Duration::from_secs(5)).unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn task_service_bounded_admission_timeout_and_failed_drain_abort_handoff() {
    let service = TaskService::disabled();
    let mut jobs = (0..4).map(|_| service.admit().unwrap()).collect::<Vec<_>>();
    assert_eq!(service.admit().err().unwrap().code, "task_busy");
    assert!(service.quiesce(Duration::from_millis(1)).is_err());
    assert_eq!(service.admit().err().unwrap().code, "task_quiescing");
    // One failed/uncertain in-flight transaction prevents commitment.
    for job in jobs.iter_mut().skip(1) {
        job.completed = true;
    }
    drop(jobs);
    assert!(service.quiesce(Duration::from_secs(1)).is_err());
    service.resume();
    let mut job = service.admit().unwrap();
    job.completed = true;
    drop(job);
    assert!(service.quiesce(Duration::from_secs(1)).is_ok());
}
#[test]
fn task_service_pending_write_drain_waits_for_response_queue_without_main_loop_callback() {
    let (root, store, service) = fixture();
    // Prepare once, then use the same OS lock as an overlapping import process.
    store
        .execute(Method::TaskList(TaskListParams::default()))
        .unwrap();
    let entries = std::fs::read_dir(root.join("config/tasks-v1"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(entries.join("lock"))
        .unwrap();
    lock.lock().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    service.start(request(), tx);
    let draining = service.clone();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        done_tx
            .send(draining.quiesce(Duration::from_secs(5)))
            .unwrap();
    });
    let deadline = Instant::now() + Duration::from_secs(1);
    while !service.admission.0.lock().unwrap().closed && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert!(done_rx.try_recv().is_err());
    drop(lock);
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    let response = receive(rx);
    assert_eq!(response["result"]["task"]["revision"], 1);
    worker.join().unwrap();
    let replacement = TaskService::new(Ok(store));
    let (tx, rx) = std::sync::mpsc::channel();
    replacement.start(request(), tx);
    assert_eq!(receive(rx)["result"]["replayed"], true);
    replacement.quiesce(Duration::from_secs(5)).unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn task_service_unavailable_is_local_not_a_terminal_runtime_failure() {
    let service = TaskService::disabled();
    let (tx, rx) = std::sync::mpsc::channel();
    service.start(request(), tx);
    assert_eq!(receive(rx)["error"]["code"], "task_unavailable");
    assert!(service.quiesce(Duration::from_millis(1)).is_ok());
}
