//! Bounded read-only filesystem and Git requests. Roots are server-local absolute paths.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FilePathParams {
    pub root: String,
    #[serde(default)]
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GitReadParams {
    pub cwd: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GitDiffParams {
    pub cwd: String,
    pub path: String,
    pub staged: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FileEntry {
    // Raw UTF-8 relative path; never use the escaped display label as a request path.
    pub path: String,
    pub directory: bool,
    pub symlink: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GitFileEntry {
    pub path: String,
    pub original_path: Option<String>,
    pub index: String,
    pub worktree: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FileReference {
    pub path: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FilePreviewParams {
    pub root: String,
    pub path: String,
    pub page: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PreviewRaster {
    pub width: u32,
    pub height: u32,
    pub original_width: u32,
    pub original_height: u32,
    pub rgb_base64: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FilePreviewContent {
    Text {
        text: String,
        truncated: bool,
    },
    Image {
        image: PreviewRaster,
    },
    Pdf {
        image: PreviewRaster,
        page: u32,
        pages: u32,
    },
    Unavailable {
        reason: String,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{Method, Request};

    #[test]
    fn advertised_read_request_shapes_are_explicit_and_round_trip() {
        for (method, params) in [
            (
                "file.list",
                serde_json::json!({"root":"/remote/repo","path":"src"}),
            ),
            (
                "file.read",
                serde_json::json!({"root":"/remote/repo","path":"src/main.rs"}),
            ),
            ("git.status", serde_json::json!({"cwd":"/remote/repo/sub"})),
            ("git.log", serde_json::json!({"cwd":"/remote/repo/sub"})),
            (
                "git.diff",
                serde_json::json!({"cwd":"/remote/repo/sub","path":"new","original_path":"old","staged":true}),
            ),
        ] {
            let json = serde_json::json!({"id":"read-1","method":method,"params":params});
            let request: Request = serde_json::from_value(json.clone()).unwrap();
            assert_eq!(crate::api::api_method_name(&request.method), method);
            assert!(!crate::api::request_changes_ui(&request));
            assert_eq!(serde_json::to_value(request).unwrap(), json);
        }
        let request: Request = serde_json::from_value(serde_json::json!({"id":"r","method":"git.diff","params":{"cwd":"/repo","path":"p","staged":false}})).unwrap();
        assert!(matches!(
            request.method,
            Method::GitDiff(GitDiffParams {
                original_path: None,
                ..
            })
        ));
    }
}
