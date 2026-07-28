use std::{
    env,
    ffi::OsString,
    io::{self, IsTerminal, Write},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::Path,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use cliclack::{confirm, spinner};
use minijinja::{Environment, context};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    WorktreeKind,
    config::{EffectiveConfig, GenerationSource},
    git::{self, Repository},
    protocol::{self, Diagnostic, Effect, ErrorBody, NextStep, Response},
    trust, ui,
};

const SCHEMA_VERSION: u32 = 1;
const DIAGNOSTIC_LIMIT: usize = 64 * 1024;
const BUILTIN_TEMPLATE: &str = r"Write a factual conventional commit message for this staged change.
Return only the commit message.
The subject must use imperative mood and be fewer than 50 characters.
Follow it with a blank line and at least two concrete bullet items.
Do not claim changes not evidenced by the staged diff.

Repository: {{ repo }}
Branch: {{ branch }}
Recent commits:
{% for subject in recent_commits %}- {{ subject }}
{% endfor %}
Staged diffstat:
{{ git_diff_stat }}

Staged diff:
{{ git_diff }}
";

#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct Invocation {
    pub message: Option<String>,
    pub stage_all: bool,
    pub dry_run: bool,
    pub json: bool,
    pub request_mode: bool,
}

pub type CommitRequestEnvelope = protocol::Request<CommitRequest>;

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommitRequest {
    pub selection: Selection,
    pub message: MessageSource,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Selection {
    Staged,
    StageAll,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum MessageSource {
    Provided { value: String },
    ConfiguredGenerator,
}

#[derive(Debug)]
struct CommandFailure {
    code: &'static str,
    message: String,
    diagnostics: Vec<Diagnostic>,
}

struct HumanMessage {
    value: String,
    generation: Option<GenerationFeedback>,
}

struct GenerationFeedback {
    elapsed: Duration,
    status_rendered: bool,
}

/// Runs commit using the human or JSON adapter.
///
/// # Errors
/// Returns an error only from human-mode planning, interaction, or execution.
pub fn run(mut invocation: Invocation) -> Result<()> {
    let mut request_id = None;
    let source = if invocation.request_mode {
        if invocation.message.is_some() || invocation.stage_all || invocation.dry_run {
            return emit_failure(
                "json.invalid_request",
                "command options are forbidden with --input-output json",
                None,
                Vec::new(),
                Vec::new(),
            );
        }
        match read_request() {
            Ok(request) => {
                request_id = request.request_id;
                if request.schema_version != SCHEMA_VERSION {
                    return emit_failure_with_id(
                        "json.unsupported_schema_version",
                        &format!(
                            "unsupported schema version {}; supported versions: [1]",
                            request.schema_version
                        ),
                        request_id,
                        Vec::new(),
                        Vec::new(),
                    );
                }
                invocation.stage_all = matches!(request.input.selection, Selection::StageAll);
                invocation.dry_run = request.input.dry_run;
                request.input.message
            }
            Err(message) => {
                return emit_failure(
                    "json.invalid_request",
                    &message,
                    None,
                    Vec::new(),
                    Vec::new(),
                );
            }
        }
    } else {
        invocation
            .message
            .clone()
            .map_or(MessageSource::ConfiguredGenerator, |value| {
                MessageSource::Provided { value }
            })
    };

    if invocation.json {
        run_json(&invocation, &source, request_id)
    } else {
        run_human(&invocation, &source)
    }
}

fn read_request() -> std::result::Result<CommitRequestEnvelope, String> {
    protocol::read_request()
}

fn run_human(invocation: &Invocation, source: &MessageSource) -> Result<()> {
    let cwd = env::current_dir().context("failed to read the current directory")?;
    let repository = git::repository(&cwd)?;
    ensure_worktree(&repository)?;
    let mut stage_all = invocation.stage_all;
    let staged = git::has_staged_changes(&repository.current().path)?;
    let dirty = has_any_changes(&repository.current().path)?;
    if stage_all && !dirty {
        bail!("nothing to commit");
    }
    if !staged && !stage_all {
        if !dirty {
            bail!("nothing to commit");
        }
        if invocation.dry_run {
            bail!("nothing is staged; stage paths with Git or pass --stage-all");
        }
        ui::ensure_interactive("nothing is staged; stage paths with Git or pass --stage-all")?;
        preview_all(&repository.current().path)?;
        let approved = ui::prompt_result(
            confirm("Stage all changes and continue?")
                .initial_value(false)
                .interact(),
            "commit cancelled",
            "failed to read staging confirmation",
        )?;
        if !approved {
            return Err(ui::declined("staging declined; no changes were staged"));
        }
        stage_all = true;
    }
    let config = preflight(&repository, source)?;
    if invocation.dry_run {
        preview_selection(&repository.current().path, stage_all)?;
        return ui::finish(ui::success_style().apply_to("Commit preflight ready."));
    }
    if stage_all {
        git::stage_all(&repository.current().path)?;
    }
    ensure_staged(&repository)?;
    preview_staged(&repository.current().path)?;
    let message = resolve_message_human(&repository, source, config.as_ref())?;
    git::commit(&repository.current().path, &message.value)?;
    let hash = git::head_commit(&repository.current().path)?;
    let rendered_message = render_commit_message(&message.value);
    match message.generation {
        Some(generation) if generation.status_rendered => ui::step(rendered_message)?,
        Some(generation) => ui::step(format!(
            "{} {}\n{rendered_message}",
            ui::heading_style().apply_to("Generated commit message:"),
            muted_elapsed(generation.elapsed)
        ))?,
        None => ui::step(format!(
            "{}\n{rendered_message}",
            ui::heading_style().apply_to("Created commit")
        ))?,
    }
    ui::finish(format!(
        "{} {}",
        ui::success_style().apply_to("Committed changes @"),
        ui::muted_style().apply_to(hash.get(..7).unwrap_or(&hash))
    ))
}

#[allow(clippy::too_many_lines)]
fn run_json(
    invocation: &Invocation,
    source: &MessageSource,
    request_id: Option<String>,
) -> Result<()> {
    let mut effects = Vec::new();
    let cwd = match env::current_dir() {
        Ok(path) => path,
        Err(error) => {
            return emit_failure_with_id(
                "repository.invalid",
                &error.to_string(),
                request_id,
                effects,
                Vec::new(),
            );
        }
    };
    let repository = match git::repository(&cwd) {
        Ok(value) => value,
        Err(error) => {
            return emit_failure_with_id(
                "repository.invalid",
                &format!("{error:#}"),
                request_id,
                effects,
                Vec::new(),
            );
        }
    };
    if let Err(error) = ensure_worktree(&repository) {
        return emit_failure_with_context(
            "repository.bare",
            &error.to_string(),
            request_id,
            &repository,
            effects,
            Vec::new(),
        );
    }
    let staged = git::has_staged_changes(&repository.current().path).unwrap_or(false);
    let dirty = has_any_changes(&repository.current().path).unwrap_or(false);
    if invocation.stage_all && !dirty {
        return emit_failure_with_context(
            "commit.nothing_to_commit",
            "nothing to commit",
            request_id,
            &repository,
            effects,
            Vec::new(),
        );
    }
    if !staged && !invocation.stage_all {
        let code = if dirty {
            "commit.nothing_staged"
        } else {
            "commit.nothing_to_commit"
        };
        return emit_failure_with_context(
            code,
            if dirty {
                "nothing is staged"
            } else {
                "nothing to commit"
            },
            request_id.clone(),
            &repository,
            effects,
            recovery_steps(invocation.request_mode, request_id.as_deref()),
        );
    }
    let config = match preflight(&repository, source) {
        Ok(value) => value,
        Err(error) => {
            let text = format!("{error:#}");
            let code = if text.contains("approval") {
                "trust.approval_required"
            } else if text.contains("generator") {
                "commit.generator_unavailable"
            } else {
                "commit.preflight_failed"
            };
            return emit_failure_with_context(
                code,
                &text,
                request_id,
                &repository,
                effects,
                Vec::new(),
            );
        }
    };
    if invocation.dry_run {
        return emit_success(
            request_id,
            &repository,
            json!({"outcome":"dry_run","ready":true,"selection": if invocation.stage_all {"stage_all"} else {"staged"}}),
            effects,
            Vec::new(),
        );
    }
    if invocation.stage_all {
        effects.push(Effect {
            action: "git.stage_all".into(),
            attempted: true,
            completed: false,
            details: None,
        });
        if let Err(error) = git::stage_all(&repository.current().path) {
            return emit_failure_with_context(
                "commit.staging_failed",
                &format!("{error:#}"),
                request_id,
                &repository,
                effects,
                Vec::new(),
            );
        }
        effects.last_mut().unwrap().completed = true;
    }
    effects.push(Effect {
        action: "commit.create".into(),
        attempted: false,
        completed: false,
        details: None,
    });
    let (message, mut diagnostics) =
        match resolve_message_json(&repository, source, config.as_ref()) {
            Ok(value) => value,
            Err(failure) => {
                return emit_failure_with_context_and_diagnostics(
                    failure.code,
                    &failure.message,
                    request_id,
                    &repository,
                    effects,
                    failure.diagnostics,
                    Vec::new(),
                );
            }
        };
    effects.last_mut().unwrap().attempted = true;
    let output = match git_commit_captured(&repository.current().path, &message) {
        Ok(output) => output,
        Err(error) => {
            return emit_failure_with_context(
                "commit.git_failed",
                &format!("{error:#}"),
                request_id,
                &repository,
                effects,
                Vec::new(),
            );
        }
    };
    diagnostics.extend(diagnostics_for("git.commit", &output));
    if !output.status.success() {
        return emit_failure_with_context_and_diagnostics(
            "commit.git_failed",
            "git commit failed",
            request_id,
            &repository,
            effects,
            diagnostics,
            Vec::new(),
        );
    }
    effects.last_mut().unwrap().completed = true;
    let hash = match git::head_commit(&repository.current().path) {
        Ok(hash) => hash,
        Err(error) => {
            return emit_failure_with_context_and_diagnostics(
                "commit.result_failed",
                &format!("commit was created but its identity could not be read: {error:#}"),
                request_id,
                &repository,
                effects,
                diagnostics,
                Vec::new(),
            );
        }
    };
    emit_success(
        request_id,
        &repository,
        json!({"outcome":"committed","commit":hash,"selection": if invocation.stage_all {"stage_all"} else {"staged"}}),
        effects,
        diagnostics,
    )
}

fn preflight(repository: &Repository, source: &MessageSource) -> Result<Option<EffectiveConfig>> {
    if matches!(source, MessageSource::Provided { .. }) {
        return Ok(None);
    }
    let config = EffectiveConfig::load(repository)?;
    let command = config
        .generation
        .command
        .as_ref()
        .context("no commit generator is configured")?;
    let template = config
        .generation
        .template
        .as_ref()
        .map_or(BUILTIN_TEMPLATE, |value| value.value.as_str());
    validate_template(template)?;
    let shared = [
        config.generation.command.as_ref(),
        config.generation.template.as_ref(),
    ]
    .into_iter()
    .flatten()
    .any(|value| value.source == GenerationSource::Shared);
    if shared && !trust::is_generation_trusted(repository, &config.generation)? {
        bail!("shared commit generator approval is required; run worktrees trust commit-approve");
    }
    let _ = command;
    Ok(Some(config))
}

fn resolve_message_human(
    repository: &Repository,
    source: &MessageSource,
    config: Option<&EffectiveConfig>,
) -> Result<HumanMessage> {
    match source {
        MessageSource::Provided { value } => validate_message(value).map(|value| HumanMessage {
            value,
            generation: None,
        }),
        MessageSource::ConfiguredGenerator => {
            let config = config.context("generator config missing")?;
            let generation_started = Instant::now();
            let spinner = io::stderr().is_terminal().then(|| {
                let elapsed = ui::muted_style().apply_to("{elapsed}");
                let template = format!("{{msg}} {elapsed}");
                let spinner = spinner().with_template(&template);
                spinner.start(ui::heading_style().apply_to("Generating commit message..."));
                spinner
            });
            if spinner.is_none() {
                ui::info(ui::heading_style().apply_to("Generating commit message..."))?;
            }
            let (message, _) = match run_generator(repository, config, false) {
                Ok(result) => result,
                Err(failure) => {
                    if let Some(spinner) = &spinner {
                        spinner.error("Failed to generate commit message");
                    } else {
                        ui::warning("Failed to generate commit message")?;
                    }
                    return Err(anyhow::anyhow!(failure.message));
                }
            };
            let elapsed = generation_started.elapsed();
            if let Some(spinner) = &spinner {
                spinner.stop(format!(
                    "{} {}",
                    ui::heading_style().apply_to("Generated commit message:"),
                    muted_elapsed(elapsed)
                ));
            }
            Ok(HumanMessage {
                value: message,
                generation: Some(GenerationFeedback {
                    elapsed,
                    status_rendered: spinner.is_some(),
                }),
            })
        }
    }
}

fn muted_elapsed(elapsed: Duration) -> impl std::fmt::Display {
    ui::muted_style().apply_to(format!("{}s", elapsed.as_secs()))
}

fn resolve_message_json(
    repository: &Repository,
    source: &MessageSource,
    config: Option<&EffectiveConfig>,
) -> std::result::Result<(String, Vec<Diagnostic>), CommandFailure> {
    match source {
        MessageSource::Provided { value } => validate_message(value)
            .map(|value| (value, Vec::new()))
            .map_err(|error| CommandFailure {
                code: "commit.invalid_message",
                message: error.to_string(),
                diagnostics: Vec::new(),
            }),
        MessageSource::ConfiguredGenerator => run_generator(
            repository,
            config.ok_or_else(|| CommandFailure {
                code: "commit.generator_unavailable",
                message: "generator config missing".into(),
                diagnostics: Vec::new(),
            })?,
            true,
        ),
    }
}

fn run_generator(
    repository: &Repository,
    config: &EffectiveConfig,
    json_mode: bool,
) -> std::result::Result<(String, Vec<Diagnostic>), CommandFailure> {
    let command = &config
        .generation
        .command
        .as_ref()
        .expect("preflight requires command")
        .value;
    let template = config
        .generation
        .template
        .as_ref()
        .map_or(BUILTIN_TEMPLATE, |value| value.value.as_str());
    let prompt = render_prompt(repository, template).map_err(|error| CommandFailure {
        code: "commit.generator_failed",
        message: format!("{error:#}"),
        diagnostics: Vec::new(),
    })?;
    let mut child = Command::new("/bin/sh")
        .args(["-c", command])
        .current_dir(&repository.current().path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(if json_mode {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .spawn()
        .map_err(|error| CommandFailure {
            code: "commit.generator_failed",
            message: error.to_string(),
            diagnostics: Vec::new(),
        })?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(prompt.as_bytes())
        .map_err(|error| CommandFailure {
            code: "commit.generator_failed",
            message: error.to_string(),
            diagnostics: Vec::new(),
        })?;
    let output = child.wait_with_output().map_err(|error| CommandFailure {
        code: "commit.generator_failed",
        message: error.to_string(),
        diagnostics: Vec::new(),
    })?;
    let diagnostics = if json_mode {
        diagnostics_for("commit.generator", &output)
    } else {
        Vec::new()
    };
    if !output.status.success() {
        return Err(CommandFailure {
            code: "commit.generator_failed",
            message: format!("commit generator failed with status {}", output.status),
            diagnostics,
        });
    }
    let message = String::from_utf8(output.stdout).map_err(|_| CommandFailure {
        code: "commit.generator_invalid_output",
        message: "commit generator produced non-UTF-8 output".into(),
        diagnostics: diagnostics.clone(),
    })?;
    match validate_message(&message) {
        Ok(value) => Ok((value, diagnostics)),
        Err(error) => Err(CommandFailure {
            code: "commit.generator_invalid_output",
            message: error.to_string(),
            diagnostics,
        }),
    }
}

fn validate_message(message: &str) -> Result<String> {
    let message = message.trim().to_owned();
    if message.is_empty() {
        bail!("commit message cannot be empty");
    }
    Ok(message)
}

fn git_commit_captured(cwd: &Path, message: &str) -> Result<Output> {
    Command::new("git")
        .args(["commit", "-m", message])
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .context("failed to start git commit")
}

fn diagnostics_for(source: &str, output: &Output) -> Vec<Diagnostic> {
    [("stdout", &output.stdout), ("stderr", &output.stderr)]
        .into_iter()
        .filter(|(_, bytes)| !bytes.is_empty())
        .map(|(stream, bytes)| {
            let retained = &bytes[..bytes.len().min(DIAGNOSTIC_LIMIT)];
            Diagnostic {
                source: source.into(),
                stream: stream.into(),
                content: String::from_utf8_lossy(retained).into_owned(),
                original_size: bytes.len(),
                truncated: bytes.len() > DIAGNOSTIC_LIMIT,
            }
        })
        .collect()
}

fn ensure_worktree(repository: &Repository) -> Result<()> {
    if repository.current().is_bare() {
        bail!("the current repository is bare; commit requires a worktree");
    }
    Ok(())
}
fn ensure_staged(repository: &Repository) -> Result<()> {
    if git::has_staged_changes(&repository.current().path)? {
        Ok(())
    } else {
        bail!("nothing to commit")
    }
}
fn has_any_changes(cwd: &Path) -> Result<bool> {
    Ok(!status_bytes(cwd)?.is_empty())
}
fn status_bytes(cwd: &Path) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .args(["status", "--porcelain=v1", "-z"])
        .current_dir(cwd)
        .output()?;
    if !output.status.success() {
        bail!("git status failed");
    }
    Ok(output.stdout)
}

fn preview_all(cwd: &Path) -> Result<()> {
    let status = status_bytes(cwd)?;
    let mut lines = Vec::new();
    for entry in status
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        lines.push(String::from_utf8_lossy(entry).into_owned());
    }
    ui::info(format!(
        "{}\n{}",
        ui::heading_style().apply_to("Changes available to stage:"),
        lines.join("\n")
    ))
}
fn preview_selection(cwd: &Path, stage_all: bool) -> Result<()> {
    if stage_all {
        preview_all(cwd)
    } else {
        preview_staged(cwd)
    }
}
fn preview_staged(cwd: &Path) -> Result<()> {
    let stat = git::staged_diff_stat(cwd)?;
    ui::info(format!(
        "{}\n{}",
        ui::heading_style().apply_to("Staged changes:"),
        colorize_diffstat(&stat)
    ))
}

fn colorize_diffstat(output: &str) -> String {
    trim_git_margin(output)
        .lines()
        .map(|line| match line.split_once(" | ") {
            Some((path, stat)) => format!("{} | {stat}", ui::worktree_data_style().apply_to(path)),
            None => ui::muted_style().apply_to(line).to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn validate_template(template: &str) -> Result<()> {
    let mut environment = Environment::new();
    environment
        .add_template("commit", template)
        .context("failed to parse commit generation template")?;
    Ok(())
}
fn render_prompt(repository: &Repository, template: &str) -> Result<String> {
    let mut environment = Environment::new();
    environment.add_template("commit", template)?;
    let branch = match &repository.current().kind {
        WorktreeKind::Branch(value) => value.as_str(),
        WorktreeKind::Detached => "(detached)",
        _ => "(unknown)",
    };
    let repo = repository.current().path.file_name().map_or_else(
        || "(unknown)".into(),
        |value| value.to_string_lossy().into_owned(),
    );
    environment.get_template("commit")?.render(context! { git_diff => git::staged_diff(&repository.current().path)?, git_diff_stat => git::staged_diff_stat(&repository.current().path)?, branch, repo, recent_commits => git::recent_subjects(&repository.current().path)? }).context("failed to render commit generation template")
}
fn render_commit_message(message: &str) -> String {
    match message.split_once('\n') {
        Some((subject, body)) => format!(
            "{}\n{body}",
            ui::worktree_data_style().bold().apply_to(subject)
        ),
        None => ui::worktree_data_style()
            .bold()
            .apply_to(message)
            .to_string(),
    }
}
fn trim_git_margin(output: &str) -> String {
    output
        .lines()
        .map(|line| line.strip_prefix(' ').unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn context_json(repository: &Repository) -> Value {
    let path = &repository.current().path;
    let status = status_bytes(path).unwrap_or_default();
    let mut staged = Vec::new();
    let mut unstaged = Vec::new();
    let mut untracked = Vec::new();
    for entry in status
        .split(|byte| *byte == 0)
        .filter(|entry| entry.len() >= 4)
    {
        let value = json!({
            "status": String::from_utf8_lossy(&entry[..2]),
            "path": path_json(&OsString::from_vec(entry[3..].to_vec()))
        });
        if &entry[..2] == b"??" {
            untracked.push(value);
        } else {
            if entry[0] != b' ' {
                staged.push(value.clone());
            }
            if entry[1] != b' ' {
                unstaged.push(value);
            }
        }
    }
    json!({"repository":{"path":path_json(path.as_os_str())},"changes":{"staged":staged,"unstaged":unstaged,"untracked":untracked,"staged_diffstat":git::staged_diff_stat(path).unwrap_or_default()}})
}
fn path_json(path: &std::ffi::OsStr) -> Value {
    match path.to_str() {
        Some(value) => json!({"encoding":"utf8","value":value}),
        None => {
            json!({"encoding":"base64","display":String::from_utf8_lossy(path.as_bytes()),"value":STANDARD.encode(path.as_bytes())})
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn response(
    request_id: Option<String>,
    status: &'static str,
    result: Option<Value>,
    error: Option<ErrorBody>,
    context: Value,
    effects: Vec<Effect>,
    diagnostics: Vec<Diagnostic>,
    next_steps: Vec<NextStep>,
) -> Response {
    Response {
        schema_version: SCHEMA_VERSION,
        request_id,
        command: "commit".into(),
        status,
        result,
        error,
        context,
        effects,
        diagnostics,
        next_steps,
    }
}
fn print_response(value: &Response) -> Result<()> {
    protocol::write(value)
}
fn emit_success(
    request_id: Option<String>,
    repository: &Repository,
    result: Value,
    effects: Vec<Effect>,
    diagnostics: Vec<Diagnostic>,
) -> Result<()> {
    print_response(&response(
        request_id,
        "success",
        Some(result),
        None,
        context_json(repository),
        effects,
        diagnostics,
        Vec::new(),
    ))
}
fn emit_failure(
    code: &str,
    message: &str,
    request_id: Option<String>,
    effects: Vec<Effect>,
    next_steps: Vec<NextStep>,
) -> Result<()> {
    emit_failure_with_id(code, message, request_id, effects, next_steps)
}
fn emit_failure_with_id(
    code: &str,
    message: &str,
    request_id: Option<String>,
    effects: Vec<Effect>,
    next_steps: Vec<NextStep>,
) -> Result<()> {
    print_response(&response(
        request_id,
        "error",
        None,
        Some(ErrorBody {
            code: code.into(),
            message: message.into(),
        }),
        json!({"repository":{},"changes":{}}),
        effects,
        Vec::new(),
        next_steps,
    ))?;
    std::process::exit(1)
}
fn emit_failure_with_context(
    code: &str,
    message: &str,
    request_id: Option<String>,
    repository: &Repository,
    effects: Vec<Effect>,
    next_steps: Vec<NextStep>,
) -> Result<()> {
    emit_failure_with_context_and_diagnostics(
        code,
        message,
        request_id,
        repository,
        effects,
        Vec::new(),
        next_steps,
    )
}
fn emit_failure_with_context_and_diagnostics(
    code: &str,
    message: &str,
    request_id: Option<String>,
    repository: &Repository,
    effects: Vec<Effect>,
    diagnostics: Vec<Diagnostic>,
    next_steps: Vec<NextStep>,
) -> Result<()> {
    print_response(&response(
        request_id,
        "error",
        None,
        Some(ErrorBody {
            code: code.into(),
            message: message.into(),
        }),
        context_json(repository),
        effects,
        diagnostics,
        next_steps,
    ))?;
    std::process::exit(1)
}

fn recovery_steps(request_mode: bool, request_id: Option<&str>) -> Vec<NextStep> {
    let invocation = |argv: Vec<&str>, stdin: Option<Value>| json!({"argv":argv,"stdin":stdin});
    let stdin = request_mode.then(|| json!({"schema_version":1,"request_id":request_id,"input":{"selection":"stage_all","message":{"source":"configured_generator"},"dry_run":false}}));
    vec![
        NextStep {
            action: "git.stage_paths".into(),
            description: "Stage selected paths with Git".into(),
            mutation: "repository".into(),
            requires_human_approval: false,
            invocation: invocation(vec!["git", "add", "<paths>"], None),
        },
        NextStep {
            action: "git.stage_patch".into(),
            description: "Interactively stage patches with Git".into(),
            mutation: "repository".into(),
            requires_human_approval: true,
            invocation: invocation(vec!["git", "add", "--patch"], None),
        },
        NextStep {
            action: "commit.stage_all".into(),
            description: "Stage every change and retry".into(),
            mutation: "repository".into(),
            requires_human_approval: false,
            invocation: invocation(
                if request_mode {
                    vec!["worktrees", "commit", "--input-output", "json"]
                } else {
                    vec!["worktrees", "commit", "--stage-all", "--output", "json"]
                },
                stdin,
            ),
        },
    ]
}

pub fn render_clap_json(args: &[OsString], error: &clap::Error) {
    let help = error.kind() == clap::error::ErrorKind::DisplayHelp;
    let version = error.kind() == clap::error::ErrorKind::DisplayVersion;
    let words: Vec<_> = args.iter().filter_map(|arg| arg.to_str()).collect();
    let commit_help = words.contains(&"commit");
    let leaf = words
        .iter()
        .find_map(|word| match *word {
            "list" | "switch" | "get" | "remove" | "merge" | "install" => Some((*word).to_owned()),
            _ => None,
        })
        .or_else(|| {
            words
                .iter()
                .position(|word| *word == "trust")
                .and_then(|index| words.get(index + 1))
                .map(|leaf| format!("trust.{}", leaf.replace('-', "_")))
        });
    let result = if help {
        if commit_help {
            json!({"outcome":"help","arguments":["--message","--stage-all","--dry-run"],"request_schema":schema_for!(CommitRequestEnvelope),"response_schema":schema_for!(Response),"error_codes":["cli.invalid_arguments","json.invalid_request","json.unsupported_schema_version","commit.nothing_staged","commit.nothing_to_commit","commit.generator_failed","commit.git_failed","trust.approval_required"],"actions":["git.stage_paths","git.stage_patch","commit.stage_all","commit.retry_staged","trust.approve_commit_generator","help.command_json"]})
        } else if let Some(command) = leaf.as_deref() {
            crate::machine::help(command)
        } else {
            json!({"outcome":"help","commands":(["list","switch","get","remove","merge","commit","trust.status","trust.reset","trust.commit_status","trust.commit_reset","trust.commit_approve","install"].into_iter().map(|name|json!({"name":name,"json_support":"full"})).collect::<Vec<_>>()),"response_schema_version":1,"supported_request_schema_versions":[1],"global_options":["--output human|json","--input-output json"]})
        }
    } else if version {
        json!({"outcome":"version","version":env!("CARGO_PKG_VERSION")})
    } else {
        Value::Null
    };
    let mut response = if help || version {
        response(
            None,
            "success",
            Some(result),
            None,
            json!({"repository":{},"changes":{}}),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
    } else {
        response(
            None,
            "error",
            None,
            Some(ErrorBody {
                code: "cli.invalid_arguments".into(),
                message: error.to_string(),
            }),
            json!({"repository":{},"changes":{}}),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
    };
    response.command = if commit_help {
        "commit".into()
    } else {
        leaf.unwrap_or_else(|| "cli".into())
    };
    let _ = print_response(&response);
}

#[cfg(test)]
mod tests {
    use super::{CommitRequestEnvelope, Response};
    #[test]
    fn schemas_are_generated_from_runtime_types() {
        assert!(
            schemars::schema_for!(CommitRequestEnvelope)
                .schema
                .object
                .is_some()
        );
        assert!(schemars::schema_for!(Response).schema.object.is_some());
    }
}
