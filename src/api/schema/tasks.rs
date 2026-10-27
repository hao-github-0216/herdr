//! Task writes are explicit reports, not authenticated authorship or executed verification.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Todo,
    Working,
    Blocked,
    Review,
    Done,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TaskVerification {
    #[default]
    Unknown,
    Pass,
    Fail,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskProvenance {
    pub reporter: Option<String>,
    pub agent: Option<String>,
    pub boot_id: Option<String>,
    pub pane_id: Option<String>,
    pub agent_session: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TaskSource {
    pub store_id: String,
    pub reported: TaskProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TaskConclusion {
    pub conclusion_id: String,
    pub text: String,
    pub recorded_at_unix_ms: u64,
    pub source: TaskSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TaskInfo {
    pub task_id: String,
    pub revision: u64,
    pub title: String,
    pub description: Option<String>,
    // Strings on reads preserve future values without interpreting them.
    pub status: String,
    pub reported_verification: String,
    pub verification_evidence: Option<String>,
    pub workspace_id: Option<String>,
    pub workspace_label: Option<String>,
    pub source: TaskSource,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
    pub conclusions: Vec<TaskConclusion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TaskSummary {
    pub task_id: String,
    pub revision: u64,
    pub title: String,
    pub status: String,
    pub reported_verification: String,
    pub workspace_id: Option<String>,
    pub workspace_label: Option<String>,
    pub updated_at_unix_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskCreateParams {
    pub nonce: String,
    pub title: String,
    pub description: Option<String>,
    #[serde(default)]
    pub status: Option<TaskStatus>,
    pub workspace_id: Option<String>,
    pub workspace_label: Option<String>,
    #[serde(default)]
    pub provenance: TaskProvenance,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskListParams {
    pub workspace_id: Option<String>,
    pub status: Option<String>,
    pub after: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskTarget {
    pub task_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskUpdateParams {
    pub task_id: String,
    pub expected_revision: u64,
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: Option<TaskStatus>,
    pub reported_verification: Option<TaskVerification>,
    pub verification_evidence: Option<String>,
    pub workspace_id: Option<String>,
    pub workspace_label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskAppendConclusionParams {
    pub task_id: String,
    pub expected_revision: u64,
    pub nonce: String,
    pub text: String,
    #[serde(default)]
    pub provenance: TaskProvenance,
}

/// Endpoint-local observation, not authenticated agent authorship or session proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskSourceContextParams {
    pub pane_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TaskSourceContextInfo {
    pub store_id: String,
    pub boot_id: String,
    pub pane_id: String,
    pub workspace_id: String,
    pub agent: Option<String>,
}
