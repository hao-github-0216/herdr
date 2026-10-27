//! One-shot bound Tasks adapter shared by native tools and context/CLI hosts.
//! This is not MCP or an authorization boundary. Tasks v1 has no atomic boot/store write fence.
use crate::api::schema::*;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

const INPUT_LIMIT: usize = 16 * 1024;
const RESULT_LIMIT: usize = 256 * 1024;
const DEADLINE: Duration = Duration::from_secs(3);
const METHODS: [&str; 6] = [
    "list",
    "get",
    "create",
    "update",
    "append_conclusion",
    "source_context",
];

#[derive(Clone)]
struct Binding {
    socket: PathBuf,
    pane: String,
    workspace: String,
}
impl Binding {
    fn capture() -> Result<Self, Value> {
        let env = |name| {
            std::env::var(name).map_err(|_| {
                failure(
                    "unavailable",
                    &format!("missing trusted launch binding {name}"),
                )
            })
        };
        let socket = env("HERDR_SOCKET_PATH")?;
        let bin = env("HERDR_BIN_PATH")?;
        let pane = env("HERDR_PANE_ID")?;
        let workspace = env("HERDR_WORKSPACE_ID")?;
        if !PathBuf::from(&bin).is_absolute()
            || std::env::current_exe()
                .ok()
                .and_then(|p| p.canonicalize().ok())
                .zip(PathBuf::from(&bin).canonicalize().ok())
                .is_none_or(|(a, b)| a != b)
            || !valid_identifier(&pane)
            || !valid_identifier(&workspace)
            || socket.is_empty()
            || socket.chars().any(char::is_control)
        {
            return Err(failure(
                "unavailable",
                "invalid exact executable/socket/pane/workspace launch binding; no fallback",
            ));
        }
        #[cfg(unix)]
        if !PathBuf::from(&socket).is_absolute() {
            return Err(failure("unavailable", "socket binding must be absolute"));
        }
        Ok(Self {
            socket: socket.into(),
            pane,
            workspace,
        })
    }
}
fn valid_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
}
// Version 1 covers binding and source-enrichment/default semantics. Bump it if
// those semantics change; agent labels are observational and do not affect writes.
fn context_fingerprint(
    binding: &Binding,
    context: &TaskSourceContextInfo,
) -> Result<String, Value> {
    let socket = binding.socket.to_str().ok_or_else(|| {
        failure(
            "unavailable",
            "launch socket must retain its exact UTF-8 spelling",
        )
    })?;
    let tuple = serde_json::to_vec(&(
        "herdr:tasks-adapter-context",
        1,
        socket,
        &context.store_id,
        &context.boot_id,
        &context.pane_id,
        &context.workspace_id,
    ))
    .map_err(|_| failure("unavailable", "cannot encode adapter context"))?;
    Ok(format!("hctx1:{:x}", Sha256::digest(tuple)))
}
fn is_mutation(name: &str) -> bool {
    matches!(name, "create" | "update" | "append_conclusion")
}
fn valid_expected_context(value: &str) -> bool {
    value.strip_prefix("hctx1:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
fn failure(code: &str, message: &str) -> Value {
    json!({"ok": false, "error": {"code": code, "message": message}})
}
fn invalid(message: &str) -> Value {
    failure("invalid_arguments", message)
}

pub(super) fn run(matches: &clap::ArgMatches) -> io::Result<i32> {
    let (action, args) = matches
        .subcommand()
        .ok_or_else(|| io::Error::other("adapter action required"))?;
    let method = if action == "discover" {
        None
    } else {
        args.get_one::<String>("method").cloned()
    };
    let binding = match Binding::capture() {
        Ok(binding) => binding,
        Err(error) => {
            println!("{error}");
            return Ok(1);
        }
    };
    // One operation per process, one worker, no retries. Process exit also ends a
    // stalled connect/read/stdin worker on platforms without socket deadlines.
    let writing = method
        .as_deref()
        .is_some_and(|m| matches!(m, "create" | "update" | "append_conclusion"));
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = (|| {
            let params = if method.is_some() {
                let mut bytes = Vec::new();
                io::stdin()
                    .lock()
                    .take(INPUT_LIMIT as u64 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|_| invalid("cannot read JSON stdin"))?;
                if bytes.len() > INPUT_LIMIT {
                    return Err(invalid("JSON stdin exceeds 16 KiB"));
                }
                serde_json::from_slice::<Value>(&bytes)
                    .map_err(|_| invalid("expected one JSON object on stdin"))?
            } else {
                json!({})
            };
            call(&binding, method.as_deref(), params)
        })();
        let _ = tx.send(result.unwrap_or_else(|error| error));
    });
    let result = rx.recv_timeout(DEADLINE).unwrap_or_else(|_| failure(
        if writing { "commit_uncertain" } else { "unavailable" },
        "adapter deadline exceeded; no automatic retry. A write may have committed; retain original nonce and payload",
    ));
    println!("{result}");
    Ok(if result["ok"] == true { 0 } else { 1 })
}

fn request(binding: &Binding, method: Method, id: &str) -> Result<ResponseResult, Value> {
    let mut stream = crate::ipc::connect_local_stream(&binding.socket)
        .map_err(|e| failure("unavailable", &e.to_string()))?;
    let bytes = serde_json::to_vec(&Request {
        id: id.into(),
        method,
    })
    .map_err(|_| invalid("cannot encode request"))?;
    stream
        .write_all(&bytes)
        .and_then(|_| stream.write_all(b"\n"))
        .map_err(|e| failure("commit_uncertain", &e.to_string()))?;
    let mut line = Vec::new();
    BufReader::new(stream)
        .take(RESULT_LIMIT as u64 + 1)
        .read_until(b'\n', &mut line)
        .map_err(|e| failure("commit_uncertain", &e.to_string()))?;
    if line.len() > RESULT_LIMIT || line.last() != Some(&b'\n') {
        return Err(failure(
            "response_uncertain",
            "oversize or incomplete response; do not blindly retry writes",
        ));
    }
    let value: Value = serde_json::from_slice(&line)
        .map_err(|_| failure("response_uncertain", "malformed response"))?;
    if value.get("id").and_then(Value::as_str) != Some(id) {
        return Err(failure("response_uncertain", "response ID mismatch"));
    }
    if value.get("error").is_some() {
        let error: ErrorResponse = strict(value).map_err(|_| {
            failure(
                "response_uncertain",
                "invalid error envelope after send; write outcome uncertain",
            )
        })?;
        return Err(json!({"ok":false,"error":error.error}));
    }
    let response: SuccessResponse = strict(value).map_err(|_| {
        failure(
            "response_uncertain",
            "invalid success envelope after send; write outcome uncertain",
        )
    })?;
    Ok(response.result)
}
fn strict<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, Value> {
    let mut unknown = false;
    let result = serde_ignored::deserialize(value, |_| unknown = true)
        .map_err(|_| invalid("schema/type mismatch"))?;
    if unknown {
        return Err(invalid("unknown or reserved field"));
    }
    Ok(result)
}
fn object(value: &Value, allowed: &[&str]) -> Result<(), Value> {
    let map = value
        .as_object()
        .ok_or_else(|| invalid("arguments must be an object"))?;
    if map.keys().any(|k| !allowed.contains(&k.as_str())) {
        return Err(invalid("unknown or reserved field"));
    }
    fn check(v: &Value) -> bool {
        match v {
            Value::String(s) => {
                s.len() <= 8192
                    && !s.chars().any(|c| {
                        (c.is_control() && !matches!(c, '\n' | '\t'))
                            || matches!(c, '\u{061c}' | '\u{200e}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
                    })
            }
            Value::Array(_) => false,
            Value::Object(o) => o.values().all(check),
            _ => true,
        }
    }
    if !check(value) {
        return Err(invalid(
            "oversize text or forbidden control/bidi characters",
        ));
    }
    Ok(())
}
fn method(
    binding: &Binding,
    context: &TaskSourceContextInfo,
    name: &str,
    mut value: Value,
) -> Result<Method, Value> {
    if is_mutation(name) {
        if !value
            .get("expected_context")
            .and_then(Value::as_str)
            .is_some_and(valid_expected_context)
        {
            return Err(invalid("expected_context required: retain hctx1:<64 lowercase hex> from discovery/source_context for this logical operation"));
        }
        if let Some(arguments) = value.as_object_mut() {
            arguments.remove("expected_context");
        }
    }
    let provenance = TaskProvenance {
        reporter: Some("herdr:adapter".into()),
        boot_id: Some(context.boot_id.clone()),
        pane_id: Some(binding.pane.clone()),
        ..Default::default()
    };
    Ok(match name {
        "source_context" => {
            object(&value, &[])?;
            Method::TaskSourceContext(TaskSourceContextParams {
                pane_id: binding.pane.clone(),
            })
        }
        "list" => {
            object(&value, &["scope", "status", "after", "limit"])?;
            let scope = value
                .as_object_mut()
                .and_then(|m| m.remove("scope"))
                .ok_or_else(|| invalid("list requires explicit scope: current or all"))?;
            let workspace_id = match scope.as_str() {
                Some("current") => Some(binding.workspace.clone()),
                Some("all") => None,
                _ => return Err(invalid("scope must be current or all")),
            };
            let mut p: TaskListParams = strict(value)?;
            if p.limit.is_some_and(|n| n == 0 || n > 50) {
                return Err(invalid("limit must be 1..50"));
            }
            if p.after.as_ref().is_some_and(|id| !valid_identifier(id))
                || p.status
                    .as_ref()
                    .is_some_and(|s| s.len() > 256 || s.chars().any(char::is_control))
            {
                return Err(invalid("invalid cursor/status"));
            }
            p.limit = Some(p.limit.unwrap_or(50));
            p.workspace_id = workspace_id;
            Method::TaskList(p)
        }
        "get" => {
            object(&value, &["task_id"])?;
            let p: TaskTarget = strict(value)?;
            if !valid_identifier(&p.task_id) {
                return Err(invalid("invalid task_id"));
            }
            Method::TaskGet(p)
        }
        "create" => {
            object(&value, &["nonce", "title", "description", "status"])?;
            let mut p: TaskCreateParams = strict(value)?;
            if !valid_identifier(&p.nonce)
                || p.title.trim().is_empty()
                || p.title.len() > 256
                || p.title.chars().any(char::is_control)
            {
                return Err(invalid(
                    "create requires title (1..256 bytes) and stable nonce",
                ));
            }
            p.workspace_id = Some(binding.workspace.clone());
            p.provenance = provenance;
            Method::TaskCreate(p)
        }
        "update" => {
            object(
                &value,
                &[
                    "task_id",
                    "expected_revision",
                    "title",
                    "description",
                    "status",
                    "reported_verification",
                    "verification_evidence",
                ],
            )?;
            if value
                .get("expected_revision")
                .and_then(Value::as_u64)
                .is_none_or(|r| r == 0)
            {
                return Err(invalid(
                    "explicit positive expected_revision required; never blind retry CAS",
                ));
            }
            let p: TaskUpdateParams = strict(value)?;
            if !valid_identifier(&p.task_id)
                || p.title.as_ref().is_some_and(|title| {
                    title.trim().is_empty()
                        || title.len() > 256
                        || title.chars().any(char::is_control)
                })
            {
                return Err(invalid("invalid task_id/title"));
            }
            if p.title.is_none()
                && p.description.is_none()
                && p.status.is_none()
                && p.reported_verification.is_none()
                && p.verification_evidence.is_none()
            {
                return Err(invalid("update requires a change"));
            }
            Method::TaskUpdate(p)
        }
        "append_conclusion" => {
            object(&value, &["task_id", "expected_revision", "nonce", "text"])?;
            let mut p: TaskAppendConclusionParams = strict(value)?;
            if p.expected_revision == 0
                || !valid_identifier(&p.task_id)
                || !valid_identifier(&p.nonce)
                || p.text.trim().is_empty()
                || p.text.len() > 4096
            {
                return Err(invalid(
                    "positive revision, stable nonce and text (1..4096 bytes) required",
                ));
            }
            p.provenance = provenance;
            Method::TaskAppendConclusion(p)
        }
        _ => return Err(invalid("method not allowlisted")),
    })
}
fn tool_schemas() -> Result<Value, Value> {
    let encode = |schema: schemars::Schema| {
        serde_json::to_value(schema)
            .map_err(|_| failure("unavailable", "cannot encode tool schema"))
    };
    let mut schemas = json!({
        "list": encode(schemars::schema_for!(TaskListParams))?,
        "get": encode(schemars::schema_for!(TaskTarget))?,
        "create": encode(schemars::schema_for!(TaskCreateParams))?,
        "update": encode(schemars::schema_for!(TaskUpdateParams))?,
        "append_conclusion": encode(schemars::schema_for!(TaskAppendConclusionParams))?,
        "source_context": {"type":"object","properties":{},"additionalProperties":false}
    });
    for name in METHODS {
        let schema = &mut schemas[name];
        schema["additionalProperties"] = json!(false);
        if let Some(properties) = schema["properties"].as_object_mut() {
            for reserved in ["workspace_id", "workspace_label", "provenance"] {
                properties.remove(reserved);
            }
        }
        if let Some(required) = schema["required"].as_array_mut() {
            required.retain(|key| {
                !matches!(
                    key.as_str(),
                    Some("workspace_id" | "workspace_label" | "provenance")
                )
            });
        }
    }
    for name in ["create", "update", "append_conclusion"] {
        schemas[name]["properties"]["expected_context"] = json!({
            "type":"string", "pattern":"^hctx1:[0-9a-f]{64}$",
            "description":"Caller-retained context_fingerprint from discovery/source_context; never refresh for an uncertain operation"
        });
        if let Some(required) = schemas[name]["required"].as_array_mut() {
            required.push(json!("expected_context"));
        }
    }
    schemas["list"]["properties"]["scope"] = json!({"type":"string","enum":["current","all"]});
    schemas["list"]["required"] = json!(["scope"]);
    schemas["list"]["properties"]["limit"] = json!({"type":"integer","minimum":1,"maximum":50});
    Ok(schemas)
}

fn call(binding: &Binding, name: Option<&str>, value: Value) -> Result<Value, Value> {
    // Validate before any endpoint access, including reserved keys.
    let dummy = TaskSourceContextInfo {
        store_id: String::new(),
        boot_id: String::new(),
        pane_id: binding.pane.clone(),
        workspace_id: binding.workspace.clone(),
        agent: None,
    };
    if let Some(name) = name {
        method(binding, &dummy, name, value.clone())?;
    }
    let context = match request(
        binding,
        Method::TaskSourceContext(TaskSourceContextParams {
            pane_id: binding.pane.clone(),
        }),
        "adapter:source",
    )? {
        ResponseResult::TaskSourceContext { context }
            if context.pane_id == binding.pane
                && context.workspace_id == binding.workspace
                && valid_identifier(&context.store_id)
                && valid_identifier(&context.boot_id) =>
        {
            context
        }
        _ => return Err(failure("unavailable", "source context mismatch")),
    };
    let fingerprint = context_fingerprint(binding, &context)?;
    if name.is_some_and(is_mutation)
        && value.get("expected_context").and_then(Value::as_str) != Some(fingerprint.as_str())
    {
        return Err(failure("stale_context", "retained context no longer matches; this attempt was not dispatched. Previous outcome is unknown: reconcile saved IDs/current records; never automatically refresh the guard, nonce, revision or payload"));
    }
    let probe = request(
        binding,
        Method::TaskList(TaskListParams {
            workspace_id: Some(binding.workspace.clone()),
            limit: Some(1),
            ..Default::default()
        }),
        "adapter:probe",
    )?;
    if !matches!(probe, ResponseResult::TaskList { ref store_id, ref tasks, .. } if store_id == &context.store_id && tasks.len() <= 1)
    {
        return Err(failure(
            "unavailable",
            "read-only Tasks probe/store mismatch",
        ));
    }
    let Some(name) = name else {
        return Ok(
            json!({"ok":true,"context_fingerprint":fingerprint,"result":{"availability":"probed", "methods":METHODS,"schemas":tool_schemas()?,"context":context,"guidance":"Explicit reporting only. Never auto-start todos or mark done/pass at agent_end. CAS requires caller revision. First obtain context_fingerprint from discovery/source_context. All mutations require caller-retained expected_context; preserve it with original nonce, payload and revision on uncertain writes. Never refresh it to retry old work. stale_context means only this attempt was not dispatched, not that a previous attempt failed. Source context is not authentication; v1 preflight is not an atomic boot/store write fence."}}),
        );
    };
    let method = method(binding, &context, name, value)?;
    let task_id = match &method {
        Method::TaskGet(p) => Some(p.task_id.clone()),
        Method::TaskUpdate(p) => Some(p.task_id.clone()),
        Method::TaskAppendConclusion(p) => Some(p.task_id.clone()),
        _ => None,
    };
    let result = request(binding, method, "adapter:call")?;
    let valid = match (&result, name) {
        (ResponseResult::TaskSourceContext { context: actual }, "source_context") => {
            actual == &context
        }
        (
            ResponseResult::TaskList {
                store_id, tasks, ..
            },
            "list",
        ) => store_id == &context.store_id && tasks.len() <= 50,
        (
            ResponseResult::TaskRecord { store_id, task, .. },
            "get" | "create" | "update" | "append_conclusion",
        ) => {
            store_id == &context.store_id
                && task.source.store_id == *store_id
                && task
                    .conclusions
                    .iter()
                    .all(|c| c.source.store_id == *store_id)
                && task_id.as_ref().is_none_or(|id| id == &task.task_id)
                && (name != "create"
                    || (task.source.reported.pane_id.as_ref() == Some(&binding.pane)
                        && task.source.reported.boot_id.as_ref() == Some(&context.boot_id)))
        }
        _ => false,
    };
    if !valid {
        return Err(failure(
            "response_uncertain",
            "response type/store/task/source mismatch; no automatic retry",
        ));
    }
    let mut envelope = json!({"ok":true,"result":result});
    if name == "source_context" {
        envelope["context_fingerprint"] = json!(fingerprint);
    }
    Ok(envelope)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn binding() -> Binding {
        Binding {
            socket: "/synthetic/unused".into(),
            pane: "pane_1".into(),
            workspace: "ws_1".into(),
        }
    }
    fn context() -> TaskSourceContextInfo {
        TaskSourceContextInfo {
            store_id: "store_1".into(),
            boot_id: "boot_1".into(),
            pane_id: "pane_1".into(),
            workspace_id: "ws_1".into(),
            agent: None,
        }
    }
    #[test]
    fn task_adapter_context_fingerprint_is_versioned_deterministic_and_binding_specific() {
        let b = binding();
        let c = context();
        let fingerprint = context_fingerprint(&b, &c).unwrap();
        assert!(valid_expected_context(&fingerprint));
        assert_eq!(
            fingerprint,
            "hctx1:31facb3bc86815d61d6c37b6efbace62a514275bc907c6064c199d62a85ad8e3"
        );
        assert_eq!(fingerprint, context_fingerprint(&b, &c).unwrap());
        for field in ["socket", "store", "boot", "pane", "workspace"] {
            let mut changed_binding = b.clone();
            let mut changed_context = c.clone();
            match field {
                "socket" => changed_binding.socket = "/synthetic/./unused".into(),
                "store" => changed_context.store_id = "store_2".into(),
                "boot" => changed_context.boot_id = "boot_2".into(),
                "pane" => {
                    changed_binding.pane = "pane_2".into();
                    changed_context.pane_id = changed_binding.pane.clone();
                }
                "workspace" => {
                    changed_binding.workspace = "ws_2".into();
                    changed_context.workspace_id = changed_binding.workspace.clone();
                }
                _ => unreachable!(),
            }
            assert_ne!(
                fingerprint,
                context_fingerprint(&changed_binding, &changed_context).unwrap(),
                "{field}"
            );
        }
        let mut label_only = c;
        label_only.agent = Some("different-label".into());
        assert_eq!(fingerprint, context_fingerprint(&b, &label_only).unwrap());
        let schemas = tool_schemas().unwrap();
        for name in ["create", "update", "append_conclusion"] {
            assert!(schemas[name]["required"]
                .as_array()
                .unwrap()
                .contains(&json!("expected_context")));
            assert_eq!(
                schemas[name]["properties"]["expected_context"]["pattern"],
                "^hctx1:[0-9a-f]{64}$"
            );
        }
    }
    #[test]
    fn task_adapter_narrow_schemas_reject_reserved_keys_before_network() {
        for (name, value) in [
            (
                "create",
                json!({"title":"x","nonce":"same","provenance":{}}),
            ),
            ("list", json!({"scope":"all","socket":"/other"})),
            (
                "update",
                json!({"task_id":"task_1","expected_revision":1,"workspace_id":"other"}),
            ),
            ("source_context", json!({"pane_id":"other"})),
            ("get", json!({"task_id":"x\u{1b}"})),
        ] {
            assert!(call(&binding(), Some(name), value).is_err());
        }
    }
    #[test]
    fn task_adapter_explicit_scope_cas_and_stable_nonce_payload() {
        let b = binding();
        let c = context();
        assert!(method(&b, &c, "list", json!({})).is_err());
        assert!(method(&b, &c, "update", json!({"task_id":"x","status":"done"})).is_err());
        assert!(method(&b, &c, "list", json!({"scope":"all","limit":51})).is_err());
        let value = json!({"title":"test","nonce":"retained","expected_context":context_fingerprint(&b,&c).unwrap()});
        let a = method(&b, &c, "create", value.clone()).unwrap();
        let b_method = method(&b, &c, "create", value).unwrap();
        assert_eq!(
            serde_json::to_value(a).unwrap(),
            serde_json::to_value(b_method).unwrap()
        );
    }
}
