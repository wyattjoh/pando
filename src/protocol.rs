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

#[derive(Clone, Debug, JsonSchema, Serialize)]
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
