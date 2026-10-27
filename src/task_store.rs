//! Disk is authoritative on every transaction, including overlapping handoff processes.
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::api::schema::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) const MAX_STORE_BYTES: usize = 8 * 1024 * 1024;
const MAX_TASK_BYTES: usize = 256 * 1024;
const MAX_TASKS: usize = 256;
const MAX_NONCES: usize = 8192;
const LOCK_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug)]
pub(crate) struct TaskError {
    pub code: &'static str,
    pub message: String,
}
pub(crate) type Result<T> = std::result::Result<T, TaskError>;
impl TaskError {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
impl From<io::Error> for TaskError {
    fn from(error: io::Error) -> Self {
        Self::new("task_unavailable", error.to_string())
    }
}
impl From<serde_json::Error> for TaskError {
    fn from(error: serde_json::Error) -> Self {
        Self::new("task_unavailable", error.to_string())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TaskStore {
    directory: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Board {
    version: u32,
    store_id: String,
    tasks: BTreeMap<String, TaskInfo>,
    nonces: BTreeMap<String, Nonce>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Nonce {
    fingerprint: String,
    task_id: String,
    conclusion_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fault {
    None,
    BeforeWrite,
    BeforeReplace,
    AfterReplace,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn new_id(prefix: &str) -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let value = format!(
        "{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    format!("{prefix}_{:x}", Sha256::digest(value.as_bytes()))
}
fn invalid(message: impl Into<String>) -> TaskError {
    TaskError::new("invalid_request", message)
}
fn bounded(value: &str, max: usize, name: &str) -> Result<()> {
    if value.len() > max || value.contains('\0') {
        return Err(invalid(format!(
            "{name} exceeds {max} bytes or contains NUL"
        )));
    }
    Ok(())
}
fn optional(value: &Option<String>, max: usize, name: &str) -> Result<()> {
    if let Some(value) = value {
        bounded(value, max, name)?;
    }
    Ok(())
}
fn provenance(value: &TaskProvenance) -> Result<()> {
    for value in [
        &value.reporter,
        &value.agent,
        &value.boot_id,
        &value.pane_id,
        &value.agent_session,
    ] {
        optional(value, 256, "provenance")?;
    }
    if value.pane_id.is_some() && value.boot_id.is_none() {
        return Err(invalid(
            "source pane requires source boot_id; provenance is self-reported",
        ));
    }
    Ok(())
}
fn validate_task(task: &TaskInfo, store_id: &str) -> Result<()> {
    bounded(&task.task_id, 128, "task_id")?;
    bounded(&task.title, 512, "title")?;
    if task.title.trim().is_empty() || task.revision == 0 || task.source.store_id != store_id {
        return Err(invalid("invalid task identity/title/revision/source"));
    }
    optional(&task.description, 8192, "description")?;
    optional(&task.verification_evidence, 4096, "verification_evidence")?;
    optional(&task.workspace_id, 256, "workspace_id")?;
    optional(&task.workspace_label, 512, "workspace_label")?;
    bounded(&task.status, 64, "status")?;
    bounded(&task.reported_verification, 64, "reported_verification")?;
    provenance(&task.source.reported)?;
    if task.conclusions.len() > 16 {
        return Err(invalid(
            "at most 16 conclusions per task; no entries are evicted",
        ));
    }
    let mut ids = std::collections::BTreeSet::new();
    for conclusion in &task.conclusions {
        bounded(&conclusion.text, 4096, "conclusion text")?;
        bounded(&conclusion.conclusion_id, 128, "conclusion_id")?;
        if conclusion.text.trim().is_empty()
            || conclusion.source.store_id != store_id
            || !ids.insert(&conclusion.conclusion_id)
        {
            return Err(invalid("invalid conclusion identity/text/source"));
        }
        provenance(&conclusion.source.reported)?;
    }
    if serde_json::to_vec(task)?.len() > MAX_TASK_BYTES {
        return Err(invalid("serialized task exceeds response bound"));
    }
    Ok(())
}

impl TaskStore {
    /// Resolve filesystem parents, not socket inodes (which change at handoff).
    /// Unix parent aliases share a board; Windows pipe-name spelling remains lossless.
    pub(crate) fn for_endpoint(namespace: &Path, endpoint: &Path) -> io::Result<Self> {
        // A namespace may not have been created yet. Only append missing normal
        // components after resolving the existing ancestor; never erase `..`.
        fn resolve_namespace(path: &Path) -> io::Result<PathBuf> {
            match path.canonicalize() {
                Ok(path) => Ok(path),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let name = path.file_name().ok_or(error)?;
                    let parent = path
                        .parent()
                        .ok_or_else(|| io::Error::other("missing namespace parent"))?;
                    // A dangling symlink is not an absent directory we may create.
                    if std::fs::symlink_metadata(path).is_ok() {
                        return Err(io::Error::other("unresolvable task namespace"));
                    }
                    Ok(resolve_namespace(parent)?.join(name))
                }
                Err(error) => Err(error),
            }
        }
        fn absolute(path: &Path) -> io::Result<PathBuf> {
            if path.is_absolute() {
                Ok(path.to_path_buf())
            } else {
                Ok(std::env::current_dir()?.join(path))
            }
        }
        let namespace = resolve_namespace(&absolute(namespace)?)?;
        let endpoint = crate::platform::task_endpoint_identity(endpoint)?;
        let key = serde_json::to_vec(&(
            namespace.as_os_str().as_encoded_bytes(),
            endpoint.as_os_str().as_encoded_bytes(),
        ))?;
        Ok(Self {
            directory: namespace
                .join("tasks-v1")
                .join(format!("{:x}", Sha256::digest(key))),
        })
    }

    fn prepare(&self) -> Result<()> {
        let parent = self
            .directory
            .parent()
            .ok_or_else(|| invalid("missing store parent"))?;
        let namespace = parent
            .parent()
            .ok_or_else(|| invalid("missing namespace"))?;
        std::fs::create_dir_all(namespace)?;
        for directory in [parent, self.directory.as_path()] {
            crate::platform::ensure_private_state_directory(directory)?;
            crate::platform::sync_parent_directory(
                directory
                    .parent()
                    .ok_or_else(|| invalid("missing parent"))?,
            )?;
        }
        Ok(())
    }

    fn lock(&self) -> Result<File> {
        self.prepare()?;
        let path = self.directory.join("lock");
        match crate::platform::create_private_task_file(&path) {
            Ok(file) => {
                file.sync_all()?;
                crate::platform::sync_parent_directory(&self.directory)?;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        if !std::fs::symlink_metadata(&path)?.file_type().is_file() {
            return Err(TaskError::new(
                "task_unavailable",
                "task lock must be a regular file; left unchanged",
            ));
        }
        let file = crate::platform::open_private_task_file(&path, true)?;
        let deadline = Instant::now() + LOCK_TIMEOUT;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(TaskError::new(
                        "task_busy",
                        "task transaction lock timed out; retry the same nonce",
                    ))
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
    }

    fn load(&self) -> Result<Board> {
        let path = self.directory.join("store.json");
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Board {
                    version: 1,
                    store_id: new_id("store"),
                    tasks: BTreeMap::new(),
                    nonces: BTreeMap::new(),
                })
            }
            Err(error) => return Err(error.into()),
        };
        if !metadata.file_type().is_file() || metadata.len() > MAX_STORE_BYTES as u64 {
            return Err(TaskError::new(
                "task_unavailable",
                "task store is not a bounded regular file; left unchanged",
            ));
        }
        let mut bytes = Vec::new();
        crate::platform::open_private_task_file(&path, false)?
            .take(MAX_STORE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_STORE_BYTES {
            return Err(TaskError::new(
                "task_unavailable",
                "task store exceeds size limit; left unchanged",
            ));
        }
        let board: Board = serde_json::from_slice(&bytes)?;
        if board.version != 1
            || board.tasks.len() > MAX_TASKS
            || board.nonces.len() > MAX_NONCES
            || board.store_id.is_empty()
            || board.store_id.len() > 128
        {
            return Err(TaskError::new(
                "task_unavailable",
                "unsupported version or invalid task store bounds; left unchanged",
            ));
        }
        for (id, task) in &board.tasks {
            if id != &task.task_id {
                return Err(TaskError::new(
                    "task_unavailable",
                    "task identity mismatch; left unchanged",
                ));
            }
            validate_task(task, &board.store_id)
                .map_err(|error| TaskError::new("task_unavailable", error.message))?;
        }
        for (key, nonce) in &board.nonces {
            if key.len() > 256
                || nonce.fingerprint.len() != 64
                || !board.tasks.contains_key(&nonce.task_id)
                || nonce.conclusion_id.as_ref().is_some_and(|id| {
                    !board.tasks[&nonce.task_id]
                        .conclusions
                        .iter()
                        .any(|c| &c.conclusion_id == id)
                })
            {
                return Err(TaskError::new(
                    "task_unavailable",
                    "invalid durable nonce record; left unchanged",
                ));
            }
        }
        Ok(board)
    }

    fn save(&self, board: &Board, fault: Fault) -> Result<()> {
        let bytes = serde_json::to_vec(board)?;
        if bytes.len() > MAX_STORE_BYTES {
            return Err(TaskError::new(
                "task_capacity",
                "task store full; no data was evicted",
            ));
        }
        if fault == Fault::BeforeWrite {
            return Err(io::Error::other("injected before write").into());
        }
        let temporary = self.directory.join(new_id("tmp"));
        let write = (|| -> Result<()> {
            let mut file = crate::platform::create_private_task_file(&temporary)?;
            file.write_all(&bytes)?;
            file.flush()?;
            file.sync_all()?;
            if fault == Fault::BeforeReplace {
                return Err(io::Error::other("injected before replace").into());
            }
            crate::platform::replace_file(&temporary, &self.directory.join("store.json"))?;
            if fault == Fault::AfterReplace {
                return Err(TaskError::new("task_commit_uncertain", "replacement committed but durability sync failed; resolve create/append with the SAME nonce and identical payload; for update re-read revision before any retry"));
            }
            crate::platform::sync_parent_directory(&self.directory).map_err(|error| TaskError::new("task_commit_uncertain", format!("replacement occurred but directory sync failed: {error}; retry create/append with SAME nonce and identical payload; re-read updates")))?;
            Ok(())
        })();
        let _ = std::fs::remove_file(temporary);
        write
    }

    pub(crate) fn source_context(
        &self,
        mut context: TaskSourceContextInfo,
    ) -> Result<ResponseResult> {
        let _lock = self.lock()?;
        let board = self.load()?;
        if !self.directory.join("store.json").exists() {
            self.save(&board, Fault::None)?;
        }
        context.store_id = board.store_id;
        Ok(ResponseResult::TaskSourceContext { context })
    }

    pub(crate) fn execute(&self, method: Method) -> Result<ResponseResult> {
        self.execute_with_fault(method, Fault::None)
    }

    pub(crate) fn execute_with_fault(
        &self,
        method: Method,
        fault: Fault,
    ) -> Result<ResponseResult> {
        let _lock = self.lock()?;
        let mut board = self.load()?;
        // Empty reads also durably allocate the store identity. No startup cache.
        let initialize = !self.directory.join("store.json").exists();
        let fingerprint = format!("{:x}", Sha256::digest(serde_json::to_vec(&method)?));
        let nonce_key = match &method {
            Method::TaskCreate(p) => Some(format!("create:{}", p.nonce)),
            Method::TaskAppendConclusion(p) => Some(format!("append:{}", p.nonce)),
            _ => None,
        };
        if let Some(key) = &nonce_key {
            let nonce = key
                .split_once(':')
                .map(|(_, value)| value)
                .unwrap_or_default();
            bounded(nonce, 128, "nonce")?;
            if nonce.is_empty() {
                return Err(invalid("nonempty explicit nonce required for safe retries"));
            }
            if let Some(saved) = board.nonces.get(key) {
                if saved.fingerprint != fingerprint {
                    return Err(TaskError::new("task_nonce_mismatch", "nonce already used with a different payload; restore the original payload or use a new nonce for a new operation"));
                }
                // Re-sync before resolving an uncertain commit, rather than falsely
                // claiming a durable replay after a previous directory-sync failure.
                let resync = crate::platform::open_private_task_file(
                    &self.directory.join("store.json"),
                    true,
                )
                .and_then(|file| file.sync_all())
                .and_then(|()| crate::platform::sync_parent_directory(&self.directory));
                resync.map_err(|error| TaskError::new("task_commit_uncertain", format!("nonce is present but durability cannot be confirmed: {error}; preserve the same nonce and payload")))?;
                return Ok(ResponseResult::TaskRecord {
                    store_id: board.store_id.clone(),
                    task: board.tasks[&saved.task_id].clone(),
                    replayed: true,
                    conclusion_id: saved.conclusion_id.clone(),
                });
            }
            if board.nonces.len() >= MAX_NONCES {
                return Err(TaskError::new(
                    "task_capacity",
                    "durable nonce capacity reached; no keys evicted",
                ));
            }
        }
        let mut mutated = false;
        let mut conclusion_id = None;
        let result = match method {
            Method::TaskList(p) => {
                let limit = p.limit.unwrap_or(50);
                if !(1..=100).contains(&limit) {
                    return Err(invalid(
                        "list limit must be 1..=100; use next_after as after",
                    ));
                }
                optional(&p.workspace_id, 256, "workspace_id")?;
                optional(&p.status, 64, "status")?;
                optional(&p.after, 128, "after")?;
                let mut selected = board
                    .tasks
                    .values()
                    .filter(|task| {
                        p.after.as_ref().is_none_or(|after| &task.task_id > after)
                            && p.workspace_id
                                .as_ref()
                                .is_none_or(|ws| task.workspace_id.as_ref() == Some(ws))
                            && p.status
                                .as_ref()
                                .is_none_or(|status| &task.status == status)
                    })
                    .take(limit + 1)
                    .collect::<Vec<_>>();
                let more = selected.len() > limit;
                selected.truncate(limit);
                let next_after = more
                    .then(|| selected.last().map(|task| task.task_id.clone()))
                    .flatten();
                let tasks = selected
                    .into_iter()
                    .map(|task| TaskSummary {
                        task_id: task.task_id.clone(),
                        revision: task.revision,
                        title: task.title.clone(),
                        status: task.status.clone(),
                        reported_verification: task.reported_verification.clone(),
                        workspace_id: task.workspace_id.clone(),
                        workspace_label: task.workspace_label.clone(),
                        updated_at_unix_ms: task.updated_at_unix_ms,
                    })
                    .collect();
                ResponseResult::TaskList {
                    store_id: board.store_id.clone(),
                    tasks,
                    next_after,
                }
            }
            Method::TaskGet(p) => record(&board, &p.task_id, None)?,
            Method::TaskCreate(p) => {
                if board.tasks.len() >= MAX_TASKS {
                    return Err(TaskError::new(
                        "task_capacity",
                        "task capacity reached; no records evicted",
                    ));
                }
                let task = TaskInfo {
                    task_id: new_id("task"),
                    revision: 1,
                    title: p.title,
                    description: p.description,
                    status: enum_string(&p.status.unwrap_or(TaskStatus::Todo))?,
                    reported_verification: "unknown".into(),
                    verification_evidence: None,
                    workspace_id: p.workspace_id,
                    workspace_label: p.workspace_label,
                    source: TaskSource {
                        store_id: board.store_id.clone(),
                        reported: p.provenance,
                    },
                    created_at_unix_ms: now_ms(),
                    updated_at_unix_ms: now_ms(),
                    conclusions: Vec::new(),
                };
                validate_task(&task, &board.store_id)?;
                let id = task.task_id.clone();
                board.tasks.insert(id.clone(), task);
                mutated = true;
                record(&board, &id, None)?
            }
            Method::TaskUpdate(p) => {
                let task = board.tasks.get_mut(&p.task_id).ok_or_else(not_found)?;
                cas(task, p.expected_revision)?;
                if p.title.is_none()
                    && p.description.is_none()
                    && p.status.is_none()
                    && p.reported_verification.is_none()
                    && p.verification_evidence.is_none()
                    && p.workspace_id.is_none()
                    && p.workspace_label.is_none()
                {
                    return Err(invalid("update requires at least one changed field"));
                }
                if let Some(value) = p.title {
                    task.title = value;
                }
                if let Some(value) = p.description {
                    task.description = Some(value);
                }
                if let Some(value) = p.status {
                    task.status = enum_string(&value)?;
                }
                if let Some(value) = p.reported_verification {
                    task.reported_verification = enum_string(&value)?;
                }
                if let Some(value) = p.verification_evidence {
                    task.verification_evidence = Some(value);
                }
                if let Some(value) = p.workspace_id {
                    task.workspace_id = Some(value);
                }
                if let Some(value) = p.workspace_label {
                    task.workspace_label = Some(value);
                }
                task.revision = task
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| invalid("revision exhausted"))?;
                task.updated_at_unix_ms = now_ms();
                validate_task(task, &board.store_id)?;
                mutated = true;
                record(&board, &p.task_id, None)?
            }
            Method::TaskAppendConclusion(p) => {
                let task = board.tasks.get_mut(&p.task_id).ok_or_else(not_found)?;
                cas(task, p.expected_revision)?;
                conclusion_id = Some(new_id("conclusion"));
                task.conclusions.push(TaskConclusion {
                    conclusion_id: conclusion_id
                        .clone()
                        .ok_or_else(|| invalid("missing conclusion ID"))?,
                    text: p.text,
                    recorded_at_unix_ms: now_ms(),
                    source: TaskSource {
                        store_id: board.store_id.clone(),
                        reported: p.provenance,
                    },
                });
                task.revision = task
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| invalid("revision exhausted"))?;
                task.updated_at_unix_ms = now_ms();
                validate_task(task, &board.store_id)?;
                mutated = true;
                record(&board, &p.task_id, conclusion_id.clone())?
            }
            _ => return Err(invalid("not a task method")),
        };
        if let (Some(key), ResponseResult::TaskRecord { task, .. }) = (nonce_key, &result) {
            board.nonces.insert(
                key,
                Nonce {
                    fingerprint,
                    task_id: task.task_id.clone(),
                    conclusion_id,
                },
            );
        }
        if mutated || initialize {
            self.save(&board, fault)?;
        }
        Ok(result)
    }
}
fn enum_string(value: &impl Serialize) -> Result<String> {
    Ok(serde_json::to_value(value)?
        .as_str()
        .ok_or_else(|| invalid("invalid enum"))?
        .to_owned())
}
fn not_found() -> TaskError {
    TaskError::new(
        "task_not_found",
        "task not found in this endpoint's store; list tasks on the intended endpoint",
    )
}
fn cas(task: &TaskInfo, expected: u64) -> Result<()> {
    if task.revision != expected {
        return Err(TaskError::new("task_revision_conflict", format!("expected revision {expected}, current revision {}; get the task and reconcile before retrying", task.revision)));
    }
    Ok(())
}
fn record(board: &Board, id: &str, conclusion_id: Option<String>) -> Result<ResponseResult> {
    bounded(id, 128, "task_id")?;
    Ok(ResponseResult::TaskRecord {
        store_id: board.store_id.clone(),
        task: board.tasks.get(id).ok_or_else(not_found)?.clone(),
        replayed: false,
        conclusion_id,
    })
}

#[cfg(test)]
mod tests;
