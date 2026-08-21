use std::{env, ffi::OsString, path::Path};

use anyhow::{Context, Result, bail};
use cliclack::confirm;
use minijinja::{Environment, context};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    WorktreeKind,
    config::{EffectiveConfig, GenerationSource},
    generator,
    git::{self, LifecycleMutation, Repository, RepositoryObservation},
    protocol::{self, Diagnostic, Effect, ErrorBody, NextStep, Response},
    render, trust, ui,
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

#[derive(Clone, Debug, JsonSchema, Serialize)]
struct ChangeEntry {
    status: String,
    path: protocol::BytePath,
}

#[derive(Clone, Debug, Default, JsonSchema, Serialize)]
struct ChangesContext {
    staged: Vec<ChangeEntry>,
    unstaged: Vec<ChangeEntry>,
    untracked: Vec<ChangeEntry>,
    staged_diffstat: String,
}

#[derive(Clone, Debug, Default, JsonSchema, Serialize)]
struct RepositoryContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<protocol::BytePath>,
}

#[derive(Clone, Debug, Default, JsonSchema, Serialize)]
struct CommitContext {
    repository: RepositoryContext,
    changes: ChangesContext,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum CommitSuccess {
    DryRun {
        ready: bool,
        selection: Selection,
    },
    Committed {
        commit: String,
        selection: Selection,
    },
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
struct CommitError {
    code: String,
    message: String,
}

impl From<CommitError> for ErrorBody {
    fn from(error: CommitError) -> Self {
        Self {
            code: error.code,
            message: error.message,
        }
    }
}

struct CommitOutcome {
    result: std::result::Result<CommitSuccess, CommitError>,
    context: CommitContext,
    effects: Vec<Effect>,
    diagnostics: Vec<Diagnostic>,
    recovery: Vec<protocol::RecoveryAction<CommitRequestEnvelope>>,
}

#[derive(Debug)]
struct CommandFailure {
    code: &'static str,
    message: String,
    diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Copy)]
enum Delivery {
    Human,
    Captured,
}

struct CommitIntent {
    selection: Selection,
    message: MessageSource,
    dry_run: bool,
}

#[derive(Clone, Copy)]
struct RecoveryContext<'a> {
    request_mode: bool,
    request_id: Option<&'a str>,
}

enum PreparedMessage {
    Provided(String),
    Generator(Box<EffectiveConfig>),
}

struct HumanMessage {
    value: String,
    generated: bool,
}

struct CommitExecution {
    outcome: CommitOutcome,
    repository_path: Option<std::path::PathBuf>,
    message: Option<HumanMessage>,
}

enum CommitOperation {
    StageAllRequired {
        execution: CommitExecution,
        repository_path: std::path::PathBuf,
    },
    Complete(CommitExecution),
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

    let intent = CommitIntent {
        selection: if invocation.stage_all {
            Selection::StageAll
        } else {
            Selection::Staged
        },
        message: source,
        dry_run: invocation.dry_run,
    };
    let recovery = RecoveryContext {
        request_mode: invocation.request_mode,
        request_id: request_id.as_deref(),
    };
    if invocation.json {
        run_json(&intent, recovery, request_id.clone())
    } else {
        run_human(intent, recovery)
    }
}

fn read_request() -> std::result::Result<CommitRequestEnvelope, String> {
    protocol::read_request()
}

fn run_human(mut intent: CommitIntent, recovery: RecoveryContext<'_>) -> Result<()> {
    loop {
        match operation(&intent, &recovery, Delivery::Human) {
            CommitOperation::StageAllRequired {
                execution,
                repository_path,
            } => {
                if intent.dry_run {
                    return finish_human(execution);
                }
                ui::ensure_interactive(
                    "nothing is staged; stage paths with Git or pass --stage-all",
                )?;
                preview_all(&repository_path)?;
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
                intent.selection = Selection::StageAll;
            }
            CommitOperation::Complete(execution) => return finish_human(execution),
        }
    }
}

fn finish_human(execution: CommitExecution) -> Result<()> {
    match execution.outcome.result {
        Ok(CommitSuccess::DryRun { selection, .. }) => {
            if let Some(path) = execution.repository_path.as_deref() {
                preview_selection(path, matches!(selection, Selection::StageAll))?;
            }
            ui::finish(ui::success_style().apply_to("Commit preflight ready."))
        }
        Ok(CommitSuccess::Committed { commit, .. }) => {
            let message = execution
                .message
                .context("commit message missing from outcome")?;
            let rendered_message = render::commit_message(&message.value);
            if message.generated {
                ui::step(rendered_message)?;
            } else {
                ui::step(format!(
                    "{}\n{rendered_message}",
                    ui::heading_style().apply_to("Commit message")
                ))?;
            }
            ui::finish(format!(
                "{} {}",
                ui::success_style().apply_to("Committed changes @"),
                ui::muted_style().apply_to(commit.get(..7).unwrap_or(&commit))
            ))
        }
        Err(error) => Err(anyhow::anyhow!(error.message)),
    }
}

fn run_json(
    intent: &CommitIntent,
    recovery: RecoveryContext<'_>,
    request_id: Option<String>,
) -> Result<()> {
    let execution = match operation(intent, &recovery, Delivery::Captured) {
        CommitOperation::StageAllRequired { execution, .. }
        | CommitOperation::Complete(execution) => execution,
    };
    render_json(execution.outcome, request_id)
}

#[allow(clippy::too_many_lines)]
fn operation(
    intent: &CommitIntent,
    recovery: &RecoveryContext<'_>,
    delivery: Delivery,
) -> CommitOperation {
    let mut outcome = CommitOutcome {
        result: Err(commit_error(
            "repository.invalid",
            "repository was not inspected",
        )),
        context: CommitContext::default(),
        effects: Vec::new(),
        diagnostics: Vec::new(),
        recovery: Vec::new(),
    };
    let cwd = match env::current_dir() {
        Ok(path) => path,
        Err(error) => {
            outcome.result = Err(commit_error("repository.invalid", error));
            return complete(outcome, None, None);
        }
    };
    let repository = match RepositoryObservation::new(&cwd).repository() {
        Ok(value) => value,
        Err(error) => {
            outcome.result = Err(commit_error("repository.invalid", format!("{error:#}")));
            return complete(outcome, None, None);
        }
    };
    let repository_path = repository.current().path.clone();
    outcome.context = context_for(&repository);
    if let Err(error) = ensure_worktree(&repository) {
        outcome.result = Err(commit_error("repository.bare", error));
        return complete(outcome, Some(repository_path), None);
    }
    let staged = match git::HistoryObservation::new(&repository_path).has_staged_changes() {
        Ok(staged) => staged,
        Err(error) => {
            outcome.result = Err(commit_error(
                "commit.preflight_failed",
                format!("{error:#}"),
            ));
            return complete(outcome, Some(repository_path), None);
        }
    };
    let dirty = match has_any_changes(&repository_path) {
        Ok(dirty) => dirty,
        Err(error) => {
            outcome.result = Err(commit_error(
                "commit.preflight_failed",
                format!("{error:#}"),
            ));
            return complete(outcome, Some(repository_path), None);
        }
    };
    if matches!(intent.selection, Selection::StageAll) && !dirty {
        outcome.result = Err(commit_error(
            "commit.nothing_to_commit",
            "nothing to commit",
        ));
        return complete(outcome, Some(repository_path), None);
    }
    if !staged && matches!(intent.selection, Selection::Staged) {
        outcome.result = Err(commit_error(
            if dirty {
                "commit.nothing_staged"
            } else {
                "commit.nothing_to_commit"
            },
            if dirty {
                "nothing is staged"
            } else {
                "nothing to commit"
            },
        ));
        outcome.recovery = recovery_steps(recovery.request_mode, recovery.request_id);
        let execution = CommitExecution {
            outcome,
            repository_path: Some(repository_path.clone()),
            message: None,
        };
        return if dirty {
            CommitOperation::StageAllRequired {
                execution,
                repository_path,
            }
        } else {
            CommitOperation::Complete(execution)
        };
    }
    let prepared = match prepare_message(&repository, &intent.message) {
        Ok(prepared) => prepared,
        Err(failure) => {
            outcome.result = Err(commit_error(failure.code, failure.message));
            outcome.diagnostics = failure.diagnostics;
            return complete(outcome, Some(repository_path), None);
        }
    };
    if intent.dry_run {
        outcome.result = Ok(CommitSuccess::DryRun {
            ready: true,
            selection: intent.selection,
        });
        return complete(outcome, Some(repository_path), None);
    }
    if matches!(intent.selection, Selection::StageAll) {
        outcome.effects.push(effect("git.stage_all", true));
        let progress = matches!(delivery, Delivery::Human)
            .then(|| ui::TimedProgress::start(true, "Staging all changes...").ok())
            .flatten();
        let staging = LifecycleMutation::new(&repository_path).stage_all();
        if let Err(error) = staging {
            if let Some(progress) = progress {
                let _ = progress.fail("Failed to stage changes");
            }
            outcome.result = Err(commit_error("commit.staging_failed", format!("{error:#}")));
            outcome.context = context_for(&repository);
            return complete(outcome, Some(repository_path), None);
        }
        if let Some(progress) = progress {
            let _ = progress.complete("Staged all changes", ui::Completion::Step);
        }
        outcome.effects.last_mut().expect("stage effect").completed = true;
        outcome.context = context_for(&repository);
    }
    if let Err(error) = ensure_staged(&repository) {
        outcome.result = Err(commit_error("commit.staging_failed", format!("{error:#}")));
        return complete(outcome, Some(repository_path), None);
    }
    if matches!(delivery, Delivery::Human) {
        let _ = preview_staged(&repository_path);
    }
    let (message, generated) = match prepared {
        PreparedMessage::Provided(message) => (message, false),
        PreparedMessage::Generator(config) => {
            outcome.effects.push(effect("commit.generate", true));
            let progress = matches!(delivery, Delivery::Human)
                .then(|| ui::TimedProgress::start(true, "Generating commit message...").ok())
                .flatten();
            let generated =
                run_generator(&repository, &config, matches!(delivery, Delivery::Captured));
            let (message, diagnostics) = match generated {
                Ok(value) => {
                    if let Some(progress) = progress {
                        let _ =
                            progress.complete("Generated commit message:", ui::Completion::Step);
                    }
                    value
                }
                Err(failure) => {
                    if let Some(progress) = progress {
                        let _ = progress.fail("Failed to generate commit message");
                    }
                    outcome.result = Err(commit_error(failure.code, failure.message));
                    outcome.diagnostics = failure.diagnostics;
                    return complete(outcome, Some(repository_path), None);
                }
            };
            outcome.diagnostics = diagnostics;
            outcome
                .effects
                .last_mut()
                .expect("generation effect")
                .completed = true;
            (message, true)
        }
    };
    outcome.effects.push(effect("commit.create", true));
    let commit = commit_transition(&repository_path, &message, delivery);
    let diagnostics = match commit {
        Ok(diagnostics) => diagnostics,
        Err(failure) => {
            outcome.result = Err(commit_error(failure.code, failure.message));
            outcome.diagnostics.extend(failure.diagnostics);
            return complete(outcome, Some(repository_path), None);
        }
    };
    outcome.diagnostics.extend(diagnostics);
    outcome.effects.last_mut().expect("commit effect").completed = true;
    outcome.context = context_for(&repository);
    let human_message = HumanMessage {
        value: message,
        generated,
    };
    match git::HistoryObservation::new(&repository_path).head_commit() {
        Ok(commit) => {
            outcome.result = Ok(CommitSuccess::Committed {
                commit,
                selection: intent.selection,
            });
            complete(outcome, Some(repository_path), Some(human_message))
        }
        Err(error) => {
            outcome.result = Err(commit_error(
                "commit.result_failed",
                format!("commit was created but its identity could not be read: {error:#}"),
            ));
            complete(outcome, Some(repository_path), None)
        }
    }
}

fn complete(
    outcome: CommitOutcome,
    repository_path: Option<std::path::PathBuf>,
    message: Option<HumanMessage>,
) -> CommitOperation {
    CommitOperation::Complete(CommitExecution {
        outcome,
        repository_path,
        message,
    })
}

fn commit_transition(
    cwd: &Path,
    message: &str,
    delivery: Delivery,
) -> std::result::Result<Vec<Diagnostic>, CommandFailure> {
    match delivery {
        Delivery::Human => {
            let progress = ui::TimedProgress::start_before_stream("Running pre-commit hooks").ok();
            let result = LifecycleMutation::new(cwd).commit(message);
            match result {
                Ok(()) => {
                    if let Some(progress) = progress {
                        let _ = progress.complete("Created commit", ui::Completion::Step);
                    }
                    Ok(Vec::new())
                }
                Err(error) => {
                    if let Some(progress) = progress {
                        let _ = progress.fail("Failed to create commit");
                    }
                    Err(CommandFailure {
                        code: "commit.git_failed",
                        message: format!("{error:#}"),
                        diagnostics: Vec::new(),
                    })
                }
            }
        }
        Delivery::Captured => {
            let transcript = git_commit_captured(cwd, message).map_err(|error| CommandFailure {
                code: "commit.git_failed",
                message: format!("{error:#}"),
                diagnostics: Vec::new(),
            })?;
            let diagnostics =
                diagnostics_for_streams("git.commit", &transcript.stdout, &transcript.stderr);
            if transcript.succeeded {
                Ok(diagnostics)
            } else {
                Err(CommandFailure {
                    code: "commit.git_failed",
                    message: "git commit failed".into(),
                    diagnostics,
                })
            }
        }
    }
}

fn effect(action: &str, attempted: bool) -> Effect {
    Effect {
        action: action.into(),
        attempted,
        completed: false,
        details: None,
    }
}

#[allow(clippy::needless_pass_by_value)]
fn commit_error(code: &str, message: impl ToString) -> CommitError {
    CommitError {
        code: code.into(),
        message: message.to_string(),
    }
}

fn render_json(outcome: CommitOutcome, request_id: Option<String>) -> Result<()> {
    let response = protocol::adapt(
        "commit",
        request_id,
        outcome.result,
        outcome.context,
        outcome.effects,
        outcome.diagnostics,
        outcome.recovery,
    )?;
    let failed = response.status == "error";
    protocol::write(&response)?;
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

fn prepare_message(
    repository: &Repository,
    source: &MessageSource,
) -> std::result::Result<PreparedMessage, CommandFailure> {
    if let MessageSource::Provided { value } = source {
        return validate_message(value)
            .map(PreparedMessage::Provided)
            .map_err(|error| CommandFailure {
                code: "commit.invalid_message",
                message: error.to_string(),
                diagnostics: Vec::new(),
            });
    }
    let config = EffectiveConfig::load(repository).map_err(|error| CommandFailure {
        code: "commit.preflight_failed",
        message: format!("{error:#}"),
        diagnostics: Vec::new(),
    })?;
    if config.generation.command.is_none() {
        return Err(CommandFailure {
            code: "commit.generator_unavailable",
            message: "no commit generator is configured".into(),
            diagnostics: Vec::new(),
        });
    }
    let template = config
        .generation
        .template
        .as_ref()
        .map_or(BUILTIN_TEMPLATE, |value| value.value.as_str());
    validate_template(template).map_err(|error| CommandFailure {
        code: "commit.preflight_failed",
        message: format!("{error:#}"),
        diagnostics: Vec::new(),
    })?;
    let shared = [
        config.generation.command.as_ref(),
        config.generation.template.as_ref(),
    ]
    .into_iter()
    .flatten()
    .any(|value| value.source == GenerationSource::Shared);
    if shared {
        let trusted =
            trust::is_generation_trusted(repository, &config.generation).map_err(|error| {
                CommandFailure {
                    code: "commit.preflight_failed",
                    message: format!("{error:#}"),
                    diagnostics: Vec::new(),
                }
            })?;
        if !trusted {
            return Err(CommandFailure {
                code: "trust.approval_required",
                message:
                    "shared commit generator approval is required; run pando trust commit-approve"
                        .into(),
                diagnostics: Vec::new(),
            });
        }
    }
    Ok(PreparedMessage::Generator(Box::new(config)))
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
    let mut process = std::process::Command::new("/bin/sh");
    process
        .args(["-c", command])
        .current_dir(&repository.current().path)
        .env("GIT_TERMINAL_PROMPT", "0");
    let output = generator::run(
        &mut process,
        prompt.as_bytes(),
        generator::Limits::default(),
    )
    .map_err(|error| {
        let diagnostics = if json_mode {
            diagnostics_for_captures("commit.generator", &error.stdout, &error.stderr)
        } else {
            Vec::new()
        };
        CommandFailure {
            code: "commit.generator_failed",
            message: error.to_string(),
            diagnostics,
        }
    })?;
    let diagnostics = if json_mode {
        diagnostics_for_captures("commit.generator", &output.stdout, &output.stderr)
    } else {
        Vec::new()
    };
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr.bytes);
        let detail = detail.trim();
        let message = if detail.is_empty() || json_mode {
            format!("commit generator failed with status {}", output.status)
        } else {
            format!(
                "commit generator failed with status {}\n{detail}",
                output.status
            )
        };
        return Err(CommandFailure {
            code: "commit.generator_failed",
            message,
            diagnostics,
        });
    }
    let message = String::from_utf8(output.stdout.bytes).map_err(|_| CommandFailure {
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

fn git_commit_captured(cwd: &Path, message: &str) -> Result<git::MutationTranscript> {
    LifecycleMutation::new(cwd).commit_captured(message)
}

fn diagnostics_for_captures(
    source: &str,
    stdout: &generator::Capture,
    stderr: &generator::Capture,
) -> Vec<Diagnostic> {
    [("stdout", stdout), ("stderr", stderr)]
        .into_iter()
        .filter(|(_, capture)| capture.original_size > 0)
        .map(|(stream, capture)| Diagnostic {
            source: source.into(),
            stream: stream.into(),
            content: String::from_utf8_lossy(&capture.bytes).into_owned(),
            original_size: capture.original_size,
            truncated: capture.truncated,
        })
        .collect()
}

fn diagnostics_for_streams(source: &str, stdout: &[u8], stderr: &[u8]) -> Vec<Diagnostic> {
    [("stdout", stdout), ("stderr", stderr)]
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
    if git::HistoryObservation::new(&repository.current().path).has_staged_changes()? {
        Ok(())
    } else {
        bail!("nothing to commit")
    }
}
fn has_any_changes(cwd: &Path) -> Result<bool> {
    Ok(!status_bytes(cwd)?.is_empty())
}
fn status_bytes(cwd: &Path) -> Result<Vec<u8>> {
    Ok(git::HistoryObservation::new(cwd).status()?.into_porcelain())
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
    let stat = git::HistoryObservation::new(cwd).staged(None)?.statistics;
    ui::info(format!(
        "{}\n{}",
        ui::heading_style().apply_to("Staged changes:"),
        render::git_output(&stat)
    ))
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
    let history = git::HistoryObservation::new(&repository.current().path);
    let staged = history.staged(None)?;
    environment.get_template("commit")?.render(context! { git_diff => staged.patch, git_diff_stat => staged.statistics, branch, repo, recent_commits => history.recent_subjects()? }).context("failed to render commit generation template")
}
fn context_for(repository: &Repository) -> CommitContext {
    let path = &repository.current().path;
    let status = git::HistoryObservation::new(path)
        .status()
        .unwrap_or_default();
    let mut changes = ChangesContext {
        staged_diffstat: git::HistoryObservation::new(path)
            .staged(None)
            .map_or_else(|_| String::new(), |staged| staged.statistics),
        ..ChangesContext::default()
    };
    for entry in status.entries {
        let value = ChangeEntry {
            status: String::from_utf8_lossy(&entry.code).into_owned(),
            path: protocol::BytePath::new(&entry.path),
        };
        if entry.is_untracked() {
            changes.untracked.push(value);
        } else {
            if entry.is_staged() {
                changes.staged.push(value.clone());
            }
            if entry.is_unstaged() {
                changes.unstaged.push(value);
            }
        }
    }
    CommitContext {
        repository: RepositoryContext {
            path: Some(protocol::BytePath::path(path)),
        },
        changes,
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
fn recovery_steps(
    request_mode: bool,
    request_id: Option<&str>,
) -> Vec<protocol::RecoveryAction<CommitRequestEnvelope>> {
    let action = |name: &str, description: &str, approval, argv: Vec<&str>, stdin| {
        protocol::RecoveryAction {
            action: name.into(),
            description: description.into(),
            mutation: protocol::MutationClass::Repository,
            requires_human_approval: approval,
            invocation: protocol::RecoveryInvocation {
                argv: argv.into_iter().map(String::from).collect(),
                stdin,
                working_directory: None,
            },
        }
    };
    let stdin = request_mode.then(|| CommitRequestEnvelope {
        schema_version: SCHEMA_VERSION,
        request_id: request_id.map(String::from),
        input: CommitRequest {
            selection: Selection::StageAll,
            message: MessageSource::ConfiguredGenerator,
            dry_run: false,
        },
    });
    vec![
        action(
            "git.stage_paths",
            "Stage selected paths with Git",
            false,
            vec!["git", "add", "<paths>"],
            None,
        ),
        action(
            "git.stage_patch",
            "Interactively stage patches with Git",
            true,
            vec!["git", "add", "--patch"],
            None,
        ),
        action(
            "commit.stage_all",
            "Stage every change and retry",
            false,
            if request_mode {
                vec!["pando", "commit", "--input-output", "json"]
            } else {
                vec!["pando", "commit", "--stage-all", "--output", "json"]
            },
            stdin,
        ),
    ]
}

pub fn render_clap_json(args: &[OsString], error: &clap::Error) {
    let help = error.kind() == clap::error::ErrorKind::DisplayHelp;
    let version = error.kind() == clap::error::ErrorKind::DisplayVersion;
    let leaf = crate::machine::command_id(args);
    let commit_help = leaf.as_deref() == Some("commit");
    let result = if help {
        if commit_help {
            json!({"outcome":"help","arguments":["--message","--stage-all","--dry-run"],"request_schema":schema_for!(CommitRequestEnvelope),"response_schema":schema_for!(Response),"error_codes":["cli.invalid_arguments","json.invalid_request","json.unsupported_schema_version","commit.nothing_staged","commit.nothing_to_commit","commit.generator_failed","commit.git_failed","trust.approval_required"],"actions":["git.stage_paths","git.stage_patch","commit.stage_all","commit.retry_staged","trust.approve_commit_generator","help.command_json"]})
        } else if let Some(command) = leaf.as_deref() {
            crate::machine::help(command)
        } else {
            json!({"outcome":"help","commands":(["list","switch","create","get","remove","merge","commit","trust.status","trust.reset","trust.commit_status","trust.commit_reset","trust.commit_approve","install"].into_iter().map(|name|json!({"name":name,"json_support":"full"})).collect::<Vec<_>>()),"response_schema_version":1,"supported_request_schema_versions":[1],"global_options":["--output human|json","--input-output json"]})
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
