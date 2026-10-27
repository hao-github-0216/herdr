//! Bounded background executor. No task I/O is performed by PTY/render/API dispatch.
use crate::api::schema::{ErrorBody, ErrorResponse, Method, Request, SuccessResponse};
use crate::task_store::{TaskError, TaskStore};
use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub(crate) fn is_task_method(method: &Method) -> bool {
    matches!(
        method,
        Method::TaskCreate(_)
            | Method::TaskList(_)
            | Method::TaskGet(_)
            | Method::TaskSourceContext(_)
            | Method::TaskUpdate(_)
            | Method::TaskAppendConclusion(_)
    )
}

#[derive(Default)]
struct Admission {
    closed: bool,
    pending: usize,
    drain_failed: bool,
}
#[derive(Clone)]
pub(crate) struct TaskService {
    store: std::result::Result<TaskStore, String>,
    admission: Arc<(Mutex<Admission>, Condvar)>,
}
struct Job {
    service: TaskService,
    completed: bool,
}
impl Drop for Job {
    fn drop(&mut self) {
        let (lock, changed) = &*self.service.admission;
        if let Ok(mut state) = lock.lock() {
            state.pending = state.pending.saturating_sub(1);
            if state.closed && !self.completed {
                state.drain_failed = true;
            }
            changed.notify_all();
        }
    }
}
impl TaskService {
    pub(crate) fn disabled() -> Self {
        Self::new(Err("task service is not configured for this runtime".into()))
    }
    pub(crate) fn for_server() -> Self {
        Self::new(
            TaskStore::for_endpoint(&crate::session::data_dir(), &crate::api::socket_path())
                .map_err(|error| error.to_string()),
        )
    }
    fn new(store: std::result::Result<TaskStore, String>) -> Self {
        Self {
            store,
            admission: Arc::new((Mutex::new(Admission::default()), Condvar::new())),
        }
    }
    fn admit(&self) -> std::result::Result<Job, TaskError> {
        let mut state = self
            .admission
            .0
            .lock()
            .map_err(|_| TaskError::new("task_unavailable", "task admission lock poisoned"))?;
        if state.closed {
            return Err(TaskError::new("task_quiescing", "task service is quiescing for handoff; retry on the replacement with the same nonce"));
        }
        if state.pending >= 4 {
            return Err(TaskError::new(
                "task_busy",
                "at most four task requests in flight; retry shortly using the same nonce",
            ));
        }
        state.pending += 1;
        Ok(Job {
            service: self.clone(),
            completed: false,
        })
    }
    pub(crate) fn start(&self, request: Request, respond_to: std::sync::mpsc::Sender<String>) {
        self.start_job(request, None, respond_to);
    }
    pub(crate) fn start_source_context(
        &self,
        request: Request,
        context: crate::task_store::Result<crate::api::schema::TaskSourceContextInfo>,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        match context {
            Ok(context) => self.start_job(request, Some(context), respond_to),
            Err(error) => {
                let _ = respond_to.send(encode_error(request.id, error));
            }
        }
    }
    fn start_job(
        &self,
        request: Request,
        context: Option<crate::api::schema::TaskSourceContextInfo>,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        let store = match &self.store {
            Ok(store) => store.clone(),
            Err(error) => {
                let _ = respond_to.send(encode_error(
                    request.id,
                    TaskError::new("task_unavailable", error),
                ));
                return;
            }
        };
        let mut job = match self.admit() {
            Ok(job) => job,
            Err(error) => {
                let _ = respond_to.send(encode_error(request.id, error));
                return;
            }
        };
        let failure_id = request.id.clone();
        let failure_tx = respond_to.clone();
        if let Err(error) = std::thread::Builder::new().name("task-transaction".into()).spawn(move || {
            let result = if matches!(request.method, Method::TaskSourceContext(_)) {
                context.ok_or_else(|| TaskError::new("task_unavailable", "runtime source context unavailable"))
                    .and_then(|context| store.source_context(context))
            } else {
                store.execute(request.method)
            };
            job.completed = result.is_ok();
            let response = match result {
                Ok(result) => serde_json::to_string(&SuccessResponse { id: request.id.clone(), result }).unwrap_or_else(|error| encode_error(request.id, TaskError::new("task_commit_uncertain", format!("response encoding failed after transaction: {error}; resolve using same nonce or re-read revision")))),
                Err(error) => encode_error(request.id, error),
            };
            let _ = respond_to.send(response);
            // Drop only after the response is queued; handoff cannot outrun an
            // acknowledged persistence transaction. No main-loop callback needed.
            drop(job);
        }) {
            let _ = failure_tx.send(encode_error(failure_id, TaskError::new("task_unavailable", error.to_string())));
        }
    }
    pub(crate) fn quiesce(&self, timeout: Duration) -> io::Result<()> {
        let (lock, changed) = &*self.admission;
        let mut state = lock
            .lock()
            .map_err(|_| io::Error::other("task admission poisoned"))?;
        if !state.closed {
            state.drain_failed = false;
        }
        state.closed = true;
        let deadline = Instant::now() + timeout;
        while state.pending > 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "task writes did not drain; handoff aborted",
                ));
            }
            let (next, _) = changed
                .wait_timeout(state, remaining)
                .map_err(|_| io::Error::other("task drain poisoned"))?;
            state = next;
        }
        if state.drain_failed {
            return Err(io::Error::other("task transaction failed while draining; handoff aborted; reconcile nonce/revision before retry"));
        }
        Ok(())
    }
    pub(crate) fn resume(&self) {
        if let Ok(mut state) = self.admission.0.lock() {
            state.closed = false;
        }
    }
}
fn encode_error(id: String, error: TaskError) -> String {
    serde_json::to_string(&ErrorResponse {
        id,
        error: ErrorBody {
            code: error.code.into(),
            message: error.message,
        },
    })
    .unwrap_or_else(|_| {
        r#"{"id":"","error":{"code":"task_unavailable","message":"response encoding failed"}}"#
            .into()
    })
}

#[cfg(test)]
mod tests;
