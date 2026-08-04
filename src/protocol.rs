use std::{
    ffi::OsStr,
    io::{self, Read, Write},
    os::unix::ffi::OsStrExt,
    path::Path,
};

use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request<I> {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub input: I,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EmptyInput {}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OptionalInputRequest<I> {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<I>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct Response {
    pub schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub command: String,
    pub status: &'static str,
    pub result: Option<Value>,
    pub error: Option<ErrorBody>,
    pub context: Value,
    pub effects: Vec<Effect>,
    pub diagnostics: Vec<Diagnostic>,
    pub next_steps: Vec<NextStep>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct Effect {
    pub action: String,
    pub attempted: bool,
    pub completed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct Diagnostic {
    pub source: String,
    pub stream: String,
    pub content: String,
    pub original_size: usize,
    pub truncated: bool,
}
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct NextStep {
    pub action: String,
    pub description: String,
    pub mutation: String,
    pub requires_human_approval: bool,
    pub invocation: Value,
}

/// A reproducible command invocation attached to a recovery action.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryInvocation<I> {
    pub argv: Vec<String>,
    pub stdin: Option<I>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<BytePath>,
}

/// The mutation domain of a recovery action.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationClass {
    None,
    Config,
    Filesystem,
    Repository,
    Setup,
    Trust,
    Worktree,
}

/// A typed recovery action supplied by a command-owned outcome.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryAction<I> {
    pub action: String,
    pub description: String,
    pub mutation: MutationClass,
    pub requires_human_approval: bool,
    pub invocation: RecoveryInvocation<I>,
}

impl MutationClass {
    const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Config => "config",
            Self::Filesystem => "filesystem",
            Self::Repository => "repository",
            Self::Setup => "setup",
            Self::Trust => "trust",
            Self::Worktree => "worktree",
        }
    }
}

impl<I: Serialize> RecoveryAction<I> {
    fn into_next_step(self) -> serde_json::Result<NextStep> {
        Ok(NextStep {
            action: self.action,
            description: self.description,
            mutation: self.mutation.as_str().into(),
            requires_human_approval: self.requires_human_approval,
            invocation: serde_json::to_value(self.invocation)?,
        })
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(tag = "encoding", rename_all = "lowercase")]
pub enum BytePath {
    Utf8 { value: String },
    Base64 { display: String, value: String },
}
impl BytePath {
    #[must_use]
    pub fn new(value: &OsStr) -> Self {
        match value.to_str() {
            Some(value) => Self::Utf8 {
                value: value.into(),
            },
            None => Self::Base64 {
                display: String::from_utf8_lossy(value.as_bytes()).into_owned(),
                value: STANDARD.encode(value.as_bytes()),
            },
        }
    }
    #[must_use]
    pub fn path(value: &Path) -> Self {
        Self::new(value.as_os_str())
    }
}

/// Reads one strict request document from stdin.
///
/// # Errors
/// Returns a descriptive protocol error for unreadable, empty, malformed, or trailing input.
pub fn read_request<I: DeserializeOwned>() -> std::result::Result<Request<I>, String> {
    let mut bytes = Vec::new();
    io::stdin()
        .read_to_end(&mut bytes)
        .map_err(|e| format!("failed to read JSON request: {e}"))?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Err("JSON request stdin is empty".into());
    }
    let mut deserializer = serde_json::Deserializer::from_slice(&bytes);
    let request = Request::deserialize(&mut deserializer)
        .map_err(|e| format!("invalid JSON request: {e}"))?;
    deserializer
        .end()
        .map_err(|e| format!("trailing JSON data: {e}"))?;
    Ok(request)
}

/// Reads a strict request whose input member may be omitted.
///
/// # Errors
/// Returns a descriptive protocol error for unreadable, empty, malformed, or trailing input.
pub fn read_optional_request<I: DeserializeOwned + Default>()
-> std::result::Result<OptionalInputRequest<I>, String> {
    let mut bytes = Vec::new();
    io::stdin()
        .read_to_end(&mut bytes)
        .map_err(|e| format!("failed to read JSON request: {e}"))?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Err("JSON request stdin is empty".into());
    }
    let mut deserializer = serde_json::Deserializer::from_slice(&bytes);
    let request = OptionalInputRequest::deserialize(&mut deserializer)
        .map_err(|e| format!("invalid JSON request: {e}"))?;
    deserializer
        .end()
        .map_err(|e| format!("trailing JSON data: {e}"))?;
    Ok(request)
}

/// Adapts one command-owned typed result into the versioned JSON response envelope.
///
/// A Rust [`Result`] is the only payload input, so adaptation cannot represent a
/// response containing both a success value and a failure.
///
/// # Errors
/// Returns an error when a command-owned result, context, or recovery invocation
/// cannot be represented as JSON.
pub fn adapt<S, F, C, I>(
    command: &str,
    request_id: Option<String>,
    result: std::result::Result<S, F>,
    context: C,
    effects: Vec<Effect>,
    diagnostics: Vec<Diagnostic>,
    recovery: Vec<RecoveryAction<I>>,
) -> serde_json::Result<Response>
where
    S: Serialize,
    F: Into<ErrorBody>,
    C: Serialize,
    I: Serialize,
{
    let context = serde_json::to_value(context)?;
    let next_steps = recovery
        .into_iter()
        .map(RecoveryAction::into_next_step)
        .collect::<serde_json::Result<Vec<_>>>()?;
    let (status, result, error) = match result {
        Ok(result) => ("success", Some(serde_json::to_value(result)?), None),
        Err(error) => ("error", None, Some(error.into())),
    };

    Ok(Response {
        schema_version: SCHEMA_VERSION,
        request_id,
        command: command.into(),
        status,
        result,
        error,
        context,
        effects,
        diagnostics,
        next_steps,
    })
}

#[must_use]
pub fn success(
    command: &str,
    request_id: Option<String>,
    result: Value,
    context: Value,
    effects: Vec<Effect>,
) -> Response {
    Response {
        schema_version: 1,
        request_id,
        command: command.into(),
        status: "success",
        result: Some(result),
        error: None,
        context,
        effects,
        diagnostics: vec![],
        next_steps: vec![],
    }
}
#[must_use]
pub fn failure(
    command: &str,
    request_id: Option<String>,
    code: &str,
    message: impl Into<String>,
) -> Response {
    Response {
        schema_version: 1,
        request_id,
        command: command.into(),
        status: "error",
        result: None,
        error: Some(ErrorBody {
            code: code.into(),
            message: message.into(),
        }),
        context: serde_json::json!({}),
        effects: vec![],
        diagnostics: vec![],
        next_steps: vec![],
    }
}
/// Writes exactly one newline-terminated response document.
///
/// # Errors
/// Returns an error when stdout cannot be encoded or written.
pub fn write(response: &Response) -> Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, response)?;
    stdout.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde::Serialize;
    use serde_json::json;

    use super::{
        BytePath, Diagnostic, Effect, ErrorBody, MutationClass, RecoveryAction, RecoveryInvocation,
        adapt,
    };

    #[derive(Serialize)]
    struct Success {
        outcome: &'static str,
    }

    #[derive(Serialize)]
    struct Context {
        ready: bool,
    }

    #[derive(Serialize)]
    struct RetryInput {
        dry_run: bool,
    }

    #[test]
    fn typed_adaptation_makes_success_and_failure_exclusive() {
        let success = adapt::<_, ErrorBody, _, RetryInput>(
            "commit",
            Some("request-1".into()),
            Ok(Success { outcome: "created" }),
            Context { ready: true },
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        assert_eq!(success.status, "success");
        assert!(success.result.is_some());
        assert!(success.error.is_none());

        let failure = adapt::<Success, _, _, RetryInput>(
            "commit",
            None,
            Err(ErrorBody {
                code: "commit.failed".into(),
                message: "commit failed".into(),
            }),
            Context { ready: false },
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        assert_eq!(failure.status, "error");
        assert!(failure.result.is_none());
        assert!(failure.error.is_some());
    }

    #[test]
    fn recovery_adaptation_retains_typed_invocation() {
        let response = adapt::<Success, _, _, _>(
            "commit",
            None,
            Err(ErrorBody {
                code: "commit.failed".into(),
                message: "commit failed".into(),
            }),
            Context { ready: false },
            vec![Effect {
                action: "git.commit".into(),
                attempted: true,
                completed: false,
                details: None,
            }],
            vec![Diagnostic {
                source: "git".into(),
                stream: "stderr".into(),
                content: "failed".into(),
                original_size: 6,
                truncated: false,
            }],
            vec![RecoveryAction {
                action: "commit.retry".into(),
                description: "Retry the commit".into(),
                mutation: MutationClass::Repository,
                requires_human_approval: true,
                invocation: RecoveryInvocation {
                    argv: vec!["pando".into(), "commit".into()],
                    stdin: Some(RetryInput { dry_run: false }),
                    working_directory: Some(BytePath::new(std::ffi::OsStr::new("/tmp/repo"))),
                },
            }],
        )
        .unwrap();

        assert_eq!(
            serde_json::to_value(response.next_steps).unwrap(),
            json!([{
                "action": "commit.retry",
                "description": "Retry the commit",
                "mutation": "repository",
                "requires_human_approval": true,
                "invocation": {
                    "argv": ["pando", "commit"],
                    "stdin": {"dry_run": false},
                    "working_directory": {"encoding": "utf8", "value": "/tmp/repo"}
                }
            }])
        );
    }

    #[test]
    fn adaptation_preserves_commit_version_one_serialization() {
        let response = adapt::<_, ErrorBody, _, RetryInput>(
            "commit",
            Some("request-1".into()),
            Ok(Success { outcome: "dry_run" }),
            json!({"repository": {}}),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();

        assert_eq!(
            serde_json::to_value(response).unwrap(),
            json!({
                "schema_version": 1,
                "request_id": "request-1",
                "command": "commit",
                "status": "success",
                "result": {"outcome": "dry_run"},
                "error": null,
                "context": {"repository": {}},
                "effects": [],
                "diagnostics": [],
                "next_steps": []
            })
        );
    }
}
