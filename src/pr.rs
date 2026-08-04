use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use minijinja::{Environment, context};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    borrow::Cow,
    env, fs,
    io::{self, IsTerminal, Read, Write},
    path::Path,
    process::Command,
};

use cliclack::confirm;
use console::{Key, Term};

mod provider;

#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum, Deserialize, JsonSchema, Serialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    #[default]
    Draft,
    Ready,
}
pub type RequestEnvelope = crate::protocol::Request<Request>;

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub description_file: Option<String>,
    #[serde(default)]
    pub status: Status,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub remote: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum PrResult {
    DryRun {
        provider: String,
        base_repository: String,
        base_branch: String,
        head_repository: String,
        head_branch: String,
        remote: String,
        draft: bool,
        push: Value,
    },
    Created {
        url: String,
        provider: String,
        base_repository: String,
        base_branch: String,
        head_repository: String,
        head_branch: String,
        remote: String,
        draft: bool,
    },
}

#[derive(Clone, Debug, Default, Serialize)]
struct PrContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dirty: Option<bool>,
}

#[derive(Clone, Debug)]
struct PrFailure {
    code: &'static str,
    message: String,
}

impl From<PrFailure> for crate::protocol::ErrorBody {
    fn from(value: PrFailure) -> Self {
        Self {
            code: value.code.into(),
            message: value.message,
        }
    }
}

struct PrOutcome {
    result: std::result::Result<PrResult, PrFailure>,
    context: PrContext,
    effects: Vec<crate::protocol::Effect>,
    diagnostics: Vec<crate::protocol::Diagnostic>,
    recovery: Vec<crate::protocol::RecoveryAction<Value>>,
}

#[allow(clippy::struct_excessive_bools)]
pub struct Invocation {
    pub title: Option<String>,
    pub description: Option<String>,
    pub description_file: Option<String>,
    pub status: Status,
    pub dry_run: bool,
    pub force: bool,
    pub yolo: bool,
    pub json: bool,
    pub request_mode: bool,
    pub remote: Option<String>,
}
/// Creates a pull request after validating repository and provider state.
///
/// # Errors
/// Returns an error when validation, provider preflight, or creation fails.
pub fn run(inv: Invocation) -> Result<()> {
    if inv.yolo && (inv.json || inv.request_mode) {
        bail!(
            "--yolo is available only with human output; use separate commit and forced PR operations"
        );
    }
    if inv.yolo {
        return render_outcome(
            execute(
                inv.title,
                body_optional(inv.description, inv.description_file)?,
                Status::Ready,
                false,
                true,
                true,
                false,
                inv.remote,
            )?,
            false,
            None,
        );
    }
    if inv.request_mode {
        if inv.title.is_some() || inv.description.is_some() || inv.description_file.is_some() {
            bail!("json.invalid_request: command options are forbidden with --input-output json");
        }
        let request: RequestEnvelope = crate::protocol::read_request()
            .map_err(|message| anyhow::anyhow!("json.invalid_request: {message}"))?;
        if request.schema_version != crate::protocol::SCHEMA_VERSION {
            return render_failure(
                "json.unsupported_schema_version",
                &format!(
                    "unsupported schema version {}; supported versions: [1]",
                    request.schema_version
                ),
                request.request_id,
            );
        }
        let r = request.input;
        if r.description_file.as_deref() == Some("-") {
            return render_failure(
                "json.invalid_request",
                "stdin is not allowed as a description-file source",
                request.request_id,
            );
        }
        if r.description.is_some() && r.description_file.is_some() {
            return render_failure(
                "json.invalid_request",
                "description conflicts with description_file",
                request.request_id,
            );
        }
        return render_outcome(
            execute(
                r.title,
                body_optional(r.description, r.description_file)?,
                r.status,
                r.dry_run,
                inv.force,
                false,
                true,
                r.remote,
            )?,
            true,
            request.request_id,
        );
    }
    let title = inv.title;
    if inv.description.is_some() && inv.description_file.is_some() {
        bail!("--description conflicts with --description-file");
    }
    render_outcome(
        execute(
            title,
            body_optional(inv.description, inv.description_file)?,
            inv.status,
            inv.dry_run,
            inv.force,
            false,
            inv.json,
            inv.remote,
        )?,
        inv.json,
        None,
    )
}
fn body_optional(desc: Option<String>, file: Option<String>) -> Result<Option<String>> {
    if let Some(f) = file {
        if f == "-" {
            let mut s = String::new();
            io::stdin().read_to_string(&mut s)?;
            Ok(Some(s))
        } else {
            Ok(Some(fs::read_to_string(f)?))
        }
    } else {
        Ok(desc)
    }
}
#[allow(
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    clippy::too_many_arguments,
    clippy::fn_params_excessive_bools
)]
fn execute(
    mut title: Option<String>,
    mut body: Option<String>,
    status: Status,
    dry: bool,
    force: bool,
    yolo: bool,
    json_mode: bool,
    requested_remote: Option<String>,
) -> Result<PrOutcome> {
    let cwd = env::current_dir()?;
    let repo = crate::git::repository(&cwd)?;
    let config = crate::config::EffectiveConfig::load(&repo)?;
    let metadata_required = title.is_none() || body.is_none();
    if metadata_required {
        let Some(generator) = config.pr_generation.command.as_ref() else {
            return fail(
                json_mode,
                "pr.generator_unavailable",
                "PR metadata generator is required when title or description is omitted; configure pr.generation.command or provide both --title and --description",
            );
        };
        if generator.source == crate::config::GenerationSource::Shared
            && !crate::trust::is_pr_generation_trusted(&repo, &config.pr_generation)?
        {
            return fail(
                json_mode,
                "trust.approval_required",
                "shared PR generator approval is required; run pando trust pr-approve",
            );
        }
    }
    let base =
        crate::git::resolve_target_branch(&repo.current().path, config.target_branch.as_deref())?;
    let head = crate::git::current_branch(&repo)?.to_owned();
    let base_remote = crate::git::branch_upstream_remote(&repo.current().path, &base)?
        .context("target branch has no upstream; cannot resolve base repository")?;
    let base_url = crate::git::remote_url(&repo.current().path, &base_remote)?;
    let push_plan =
        match crate::git::plan_push(&repo.current().path, &head, requested_remote.as_deref()) {
            Ok(plan) => plan,
            Err(error)
                if requested_remote.is_none()
                    && !json_mode
                    && !force
                    && error.to_string().contains("multiple Git remotes") =>
            {
                crate::ui::ensure_interactive("remote selection requires confirmation")?;
                let remotes = git_cmd(&repo, &["remote"])?;
                let options: Vec<(String, String, String)> = remotes
                    .lines()
                    .map(|v| (v.to_owned(), v.to_owned(), String::new()))
                    .collect();
                let remote = cliclack::select("Select the pull request head remote")
                    .items(&options)
                    .interact()
                    .map_err(|e| anyhow::anyhow!(e))?;
                crate::git::plan_push(&repo.current().path, &head, Some(&remote))?
            }
            Err(error) => return fail(json_mode, "pr.remote_selection", &format!("{error:#}")),
        };
    let head_url = crate::git::remote_url(&repo.current().path, &push_plan.remote)?;
    let mut resolved_provider =
        match provider::resolve(config.pr_provider, &base_url, &head_url, &head) {
            Ok(provider) => provider,
            Err(error) => return fail(json_mode, "provider.unsupported", &format!("{error:#}")),
        };
    let provider_name = resolved_provider.adapter.name();
    let base_repo = resolved_provider.base_repository.clone();
    let head_owner = resolved_provider.head_owner.clone();
    let head_ref = resolved_provider.head_ref.clone();
    if head == base {
        return fail(
            json_mode,
            "pr.invalid_source",
            "current branch is the configured target branch",
        );
    }
    let interactive = io::stdin().is_terminal() && io::stderr().is_terminal();
    if !(force || dry || json_mode || interactive) {
        return fail(
            false,
            "pr.approval_required",
            "non-interactive creation requires --force",
        );
    }
    if let Err(error) = resolved_provider.adapter.ensure_ready(&repo.current().path) {
        return fail(json_mode, "provider.unauthenticated", &format!("{error:#}"));
    }
    let pull_request_ref = provider::PullRequestRef {
        base_repository: &base_repo,
        base_branch: &base,
        head_ref: &head_ref,
    };
    let existing = match resolved_provider
        .adapter
        .find_open(&repo.current().path, &pull_request_ref)
    {
        Ok(existing) => existing,
        Err(error) => {
            return fail(
                json_mode,
                "provider.preflight_failed",
                &format!("{error:#}"),
            );
        }
    };
    if let Some(url) = existing {
        return fail(
            json_mode,
            "pr.already_exists",
            &format!("an open pull request already exists: {url}"),
        );
    }
    let dirty = crate::git::is_dirty(&repo.current().path)?;
    if dirty {
        if force && !yolo {
            return fail_dirty(json_mode);
        }
        if yolo {
            crate::commit::run(crate::commit::Invocation {
                message: None,
                stage_all: true,
                dry_run: false,
                json: false,
                request_mode: false,
            })?;
        } else {
            let options = [
                ("commit", "Commit all changes", ""),
                ("skip", "Skip local changes", ""),
                ("stop", "Stop", ""),
            ];
            loop {
                let choice = cliclack::select("This worktree has uncommitted changes")
                    .items(&options)
                    .initial_value("commit")
                    .interact()?;
                match choice {
                    "commit" => {
                        crate::commit::run(crate::commit::Invocation {
                            message: None,
                            stage_all: false,
                            dry_run: false,
                            json: false,
                            request_mode: false,
                        })?;
                        if !crate::git::is_dirty(&repo.current().path)? {
                            break;
                        }
                    }
                    "skip" => break,
                    _ => {
                        return Err(crate::ui::declined_noop(
                            "Pull request creation cancelled; nothing was pushed or created.",
                            "Pull request creation cancelled.",
                        ));
                    }
                }
            }
        }
    }
    if metadata_required && !dry {
        let (generated_title, generated_body) = match generate_metadata(
            &repo,
            &config,
            &base,
            &head,
            title.as_deref(),
            body.as_deref(),
            json_mode,
        ) {
            Ok(metadata) => metadata,
            Err(error) => {
                return Ok(PrOutcome {
                    result: Err(PrFailure {
                        code: "pr.generator_failed",
                        message: format!("{error:#}"),
                    }),
                    context: PrContext {
                        base: Some(base),
                        head: Some(head),
                        dirty: None,
                    },
                    effects: Vec::new(),
                    diagnostics: diagnostic("pr.generator", &format!("{error:#}")),
                    recovery: Vec::new(),
                });
            }
        };
        if title.is_none() {
            title = Some(generated_title);
        }
        if body.is_none() {
            body = Some(generated_body);
        }
    }
    let mut title = title.context("PR title is required")?;
    let mut body = body.unwrap_or_default();
    if title.trim().is_empty() {
        bail!("PR title cannot be empty");
    }
    if !force && !dry && !json_mode {
        let (updated_title, updated_body) = review_metadata(
            title,
            body,
            &base_repo,
            &base,
            &head_owner,
            &head,
            &push_plan.remote,
            status,
        )?;
        title = updated_title;
        body = updated_body;
    }
    let push_effect = json!({
        "action": "git.push",
        "remote": push_plan.remote,
        "branch": push_plan.branch,
        "set_upstream": push_plan.set_upstream,
        "attempted": !dry,
        "completed": false,
    });
    if dry {
        return Ok(PrOutcome {
            result: Ok(PrResult::DryRun {
                provider: provider_name.into(),
                base_repository: base_repo,
                base_branch: base.clone(),
                head_repository: head_owner,
                head_branch: head.clone(),
                remote: push_plan.remote,
                draft: status == Status::Draft,
                push: push_effect,
            }),
            context: PrContext {
                base: Some(base),
                head: Some(head),
                dirty: None,
            },
            effects: vec![
                effect("git.push", false, false, None),
                effect("provider.create", false, false, None),
            ],
            diagnostics: Vec::new(),
            recovery: Vec::new(),
        });
    }
    let push = crate::ui::run_timed(
        !json_mode,
        "Publishing topic branch...",
        "Published topic branch",
        "Failed to publish topic branch",
        |animated| crate::git::push(&repo.current().path, &push_plan, !json_mode && !animated),
    );
    if let Err(error) = push {
        return Ok(PrOutcome {
            result: Err(PrFailure {
                code: "git.push_failed",
                message: format!("{error:#}"),
            }),
            context: PrContext {
                base: Some(base),
                head: Some(head),
                dirty: None,
            },
            effects: vec![effect("git.push", true, false, Some(push_effect))],
            diagnostics: diagnostic("git.push", &format!("{error:#}")),
            recovery: vec![retry_action()],
        });
    }
    let create_request = provider::CreatePullRequest {
        target: pull_request_ref,
        base_remote: &base_remote,
        title: &title,
        body: &body,
        status,
    };
    let creation = crate::ui::run_timed(
        !json_mode,
        "Creating pull request...",
        "Created pull request",
        "Failed to create pull request",
        |_| {
            resolved_provider
                .adapter
                .create(&repo.current().path, &create_request)
        },
    );
    let url = match creation {
        Ok(url) => url,
        Err(error) => {
            return Ok(PrOutcome {
                result: Err(PrFailure {
                    code: "provider.creation_failed",
                    message: format!("{error:#}"),
                }),
                context: PrContext {
                    base: Some(base),
                    head: Some(head),
                    dirty: None,
                },
                effects: vec![
                    effect("git.push", true, true, Some(push_effect)),
                    effect("provider.create", true, false, None),
                ],
                diagnostics: diagnostic("provider.create", &format!("{error:#}")),
                recovery: vec![retry_action()],
            });
        }
    };
    Ok(PrOutcome {
        result: Ok(PrResult::Created {
            url: url.clone(),
            provider: provider_name.into(),
            base_repository: base_repo,
            base_branch: base.clone(),
            head_repository: head_owner,
            head_branch: head.clone(),
            remote: push_plan.remote,
            draft: status == Status::Draft,
        }),
        context: PrContext {
            base: Some(base),
            head: Some(head),
            dirty: None,
        },
        effects: vec![
            effect("git.push", true, true, Some(push_effect)),
            effect("provider.create", true, true, None),
        ],
        diagnostics: Vec::new(),
        recovery: Vec::new(),
    })
}
#[allow(clippy::too_many_arguments)]
fn generate_metadata(
    repo: &crate::git::Repository,
    config: &crate::config::EffectiveConfig,
    base: &str,
    head: &str,
    title: Option<&str>,
    body: Option<&str>,
    json_mode: bool,
) -> Result<(String, String)> {
    let generator = config
        .pr_generation
        .command
        .as_ref()
        .context("PR metadata generator configuration disappeared after preflight")?;
    let pull_request_template = resolved_pull_request_template(repo, config)?;
    let repo_name = repo
        .current()
        .path
        .file_name()
        .map_or("(unknown)".into(), |value| {
            value.to_string_lossy().into_owned()
        });
    let diffstat = git_cmd(repo, &["diff", "--stat", &format!("{base}...HEAD")])?;
    let diff = git_cmd(repo, &["diff", &format!("{base}...HEAD")])?;
    let subjects = git_cmd(repo, &["log", "--format=%s", &format!("{base}..HEAD")])?;
    let explicit_title = title.unwrap_or("");
    let explicit_description = body.unwrap_or("");
    let prompt = if let Some(template) = config.pr_generation.template.as_ref() {
        let mut environment = Environment::new();
        environment.add_template("pr", &template.value)?;
        environment.get_template("pr")?.render(context! {
            repo => repo_name, branch => head, base, git_diff_stat => diffstat,
            git_diff => diff, git_commit_subjects => subjects,
            explicit_title, explicit_description, pull_request_template
        })?
    } else {
        format!(
            "Generate a PR metadata document. Return exactly one first-line level-one heading, followed by the description. Preserve required headings, checklists, and sections from the pull-request template; replace placeholders and instructional comments with factual content.\nRepository: {repo_name}\nTopic branch: {head}\nTarget branch: {base}\nDiffstat:\n{diffstat}\nCommitted commit subjects:\n{subjects}\nExplicit title: {explicit_title}\nExplicit description:\n{explicit_description}\nDiff:\n{diff}\nPull-request template:\n{pull_request_template}\n"
        )
    };
    crate::ui::run_timed(
        !json_mode,
        "Generating pull request metadata...",
        "Generated pull request metadata:",
        "Failed to generate pull request metadata",
        |_| {
            generate_with_retries(&prompt, |attempt_prompt| {
                run_generator(&generator.value, &repo.current().path, attempt_prompt)
            })
        },
    )
}

/// Number of times the PR metadata generator is invoked before giving up.
///
/// Generators are language models, so a rejected document is usually a
/// one-off formatting slip rather than a persistent failure.
const GENERATION_ATTEMPTS: u32 = 3;

/// Runs `attempt` until it produces a usable document or
/// [`GENERATION_ATTEMPTS`] is exhausted.
///
/// Every failure is retried — a nonzero exit and a malformed document are
/// equally transient for a language-model generator. Each retry re-sends the
/// prompt with the previous rejection appended, so the generator can correct
/// itself rather than reroll blindly. Retries are silent because the caller's
/// progress indicator owns the only terminal state; the attempt count is
/// reported on the final error instead.
fn generate_with_retries(
    prompt: &str,
    mut attempt: impl FnMut(&str) -> Result<(String, String)>,
) -> Result<(String, String)> {
    let mut remaining = GENERATION_ATTEMPTS;
    let mut previous: Option<anyhow::Error> = None;
    loop {
        let attempt_prompt = match &previous {
            None => Cow::Borrowed(prompt),
            Some(error) => Cow::Owned(format!(
                "{prompt}\nThe previous attempt was rejected: {error:#}. Return only the corrected document, beginning with a single line of the form \"# Title\" and followed by the description.\n"
            )),
        };
        remaining -= 1;
        match attempt(&attempt_prompt) {
            Ok(metadata) => return Ok(metadata),
            Err(error) if remaining > 0 => previous = Some(error),
            Err(error) => {
                return Err(error.context(format!(
                    "PR metadata generation failed after {GENERATION_ATTEMPTS} attempts"
                )));
            }
        }
    }
}

fn run_generator(command: &str, dir: &Path, prompt: &str) -> Result<(String, String)> {
    let mut child = Command::new("/bin/sh")
        .args(["-c", command])
        .current_dir(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .context("generator stdin unavailable")?
        .write_all(prompt.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!("PR metadata generator failed");
    }
    parse_metadata(
        &String::from_utf8(output.stdout)
            .map_err(|_| anyhow::anyhow!("PR generator produced non-UTF-8 output"))?,
    )
}

#[allow(clippy::too_many_arguments)]
fn review_metadata(
    mut title: String,
    mut body: String,
    base_repo: &str,
    base: &str,
    head_repo: &str,
    head: &str,
    remote: &str,
    status: Status,
) -> Result<(String, String)> {
    loop {
        let status = if status == Status::Draft {
            "draft"
        } else {
            "ready"
        };
        let preview = format!(
            "{}\nbase: {base_repo} ({base})\nhead: {head_repo} ({head})\npush remote: {remote}\nstatus: {status}\n\n{}\n{}",
            crate::ui::heading_style().apply_to("Review pull request"),
            crate::ui::heading_style().apply_to(format!("# {title}")),
            render_markdown(&body)
        );
        crate::ui::step(preview)?;
        crate::ui::finish(
            crate::ui::muted_style()
                .apply_to("Press Enter to create, Ctrl-G to edit, or Escape to cancel."),
        )?;
        match Term::stderr()
            .read_key()
            .context("failed to read PR review input")?
        {
            Key::Enter => {
                let confirmed = crate::ui::prompt_result(
                    confirm("Create this pull request?")
                        .initial_value(false)
                        .interact(),
                    "pull request creation cancelled",
                    "failed to read pull request confirmation",
                )?;
                if confirmed {
                    return Ok((title, body));
                }
                return Err(crate::ui::declined_noop(
                    "Pull request creation declined; nothing was pushed or created.",
                    "Pull request creation cancelled.",
                ));
            }
            Key::Escape | Key::CtrlC => {
                return Err(crate::ui::declined_noop(
                    "Pull request creation cancelled; nothing was pushed or created.",
                    "Pull request creation cancelled.",
                ));
            }
            Key::Char('\u{7}') => {
                let path = env::temp_dir().join(format!("pando-pr-{}.md", std::process::id()));
                fs::write(&path, format!("# {title}\n\n{body}\n"))?;
                let editor = resolve_editor()?;
                let status = Command::new("/bin/sh")
                    .args(["-c", &format!(r#"{editor} "$1""#)])
                    .arg("pando-pr-editor")
                    .arg(&path)
                    .status()
                    .context("failed to launch configured editor")?;
                if !status.success() {
                    let _ = fs::remove_file(&path);
                    bail!("pr.editor_failed: configured editor exited unsuccessfully");
                }
                let edited =
                    fs::read_to_string(&path).context("failed to read edited PR document")?;
                let parsed = parse_metadata(&edited).context(
                    "edited PR document is invalid; expected '# <title>' followed by a description",
                )?;
                title = parsed.0;
                body = parsed.1;
                fs::remove_file(path)?;
            }
            _ => {}
        }
    }
}

fn resolve_editor() -> Result<String> {
    if let Some(editor) = env::var_os("GIT_EDITOR").filter(|v| !v.is_empty()) {
        return Ok(editor.to_string_lossy().into_owned());
    }
    let config = Command::new("git")
        .args(["config", "--get", "core.editor"])
        .output()?;
    if config.status.success() {
        let value = String::from_utf8_lossy(&config.stdout).trim().to_owned();
        if !value.is_empty() {
            return Ok(value);
        }
    }
    for name in ["VISUAL", "EDITOR"] {
        if let Some(editor) = env::var_os(name).filter(|v| !v.is_empty()) {
            return Ok(editor.to_string_lossy().into_owned());
        }
    }
    bail!("pr.editor_missing: configure core.editor, GIT_EDITOR, VISUAL, or EDITOR")
}

fn render_markdown(body: &str) -> String {
    body.lines()
        .map(|line| {
            if line.starts_with('#') || line.starts_with("```") {
                crate::ui::heading_style().apply_to(line).to_string()
            } else if line.starts_with("- ") || line.starts_with("* ") {
                crate::ui::worktree_data_style().apply_to(line).to_string()
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn resolved_pull_request_template(
    repo: &crate::git::Repository,
    config: &crate::config::EffectiveConfig,
) -> Result<String> {
    if let Some(value) = config.pull_request_template.as_ref()
        && value.source != crate::config::GenerationSource::Global
    {
        return Ok(value.value.clone());
    }
    for path in [
        ".github/pull_request_template.md",
        ".github/PULL_REQUEST_TEMPLATE.md",
        "pull_request_template.md",
        "PULL_REQUEST_TEMPLATE.md",
    ] {
        let output = Command::new("git")
            .args(["show", &format!("HEAD:{path}")])
            .current_dir(&repo.current().path)
            .output()?;
        if output.status.success() {
            return String::from_utf8(output.stdout)
                .context("repository pull-request template is not UTF-8");
        }
    }
    Ok(config
        .pull_request_template
        .as_ref()
        .map_or_else(String::new, |value| value.value.clone()))
}

fn git_cmd(repo: &crate::git::Repository, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(&repo.current().path)
        .output()?;
    if !output.status.success() {
        bail!("git command failed");
    }
    String::from_utf8(output.stdout).context("git output was not UTF-8")
}

/// Strips a fence that wraps the *entire* document, which generators
/// routinely add when asked for markdown. A fence opening a description is
/// left alone, so only a leading fence with a matching final closing line is
/// removed.
fn unwrap_code_fence(value: &str) -> &str {
    let (first, rest) = value.split_once('\n').unwrap_or((value, ""));
    if !first.starts_with("```") {
        return value;
    }
    rest.trim_end()
        .rsplit_once('\n')
        .filter(|(_, last)| last.trim() == "```")
        .map_or(value, |(inner, _)| inner)
}

fn parse_metadata(value: &str) -> Result<(String, String)> {
    let mut lines = unwrap_code_fence(value.trim()).trim().lines();
    let first = lines
        .next()
        .context("PR generator output is missing a title")?;
    let title = first
        .strip_prefix("# ")
        .context("PR generator output must begin with exactly one level-one heading")?
        .trim();
    if title.is_empty() || title.starts_with('#') {
        bail!("PR title cannot be empty");
    }
    let description = lines.collect::<Vec<_>>().join("\n").trim().to_owned();
    if description.is_empty() {
        bail!("generated PR description cannot be empty");
    }
    Ok((title.to_owned(), description))
}

fn render_outcome(outcome: PrOutcome, json_mode: bool, request_id: Option<String>) -> Result<()> {
    if json_mode {
        let response = crate::protocol::adapt(
            "pr.create",
            request_id,
            outcome.result,
            outcome.context,
            outcome.effects,
            outcome.diagnostics,
            outcome.recovery,
        )?;
        crate::protocol::write(&response)?;
        return Ok(());
    }
    match outcome.result {
        Ok(PrResult::Created { url, .. }) => {
            crate::ui::finish(crate::ui::success_style().apply_to(url))
        }
        Ok(PrResult::DryRun { .. }) => {
            crate::ui::finish(crate::ui::success_style().apply_to("Pull request dry-run complete."))
        }
        Err(error) => bail!("{}: {}", error.code, error.message),
    }
}

fn render_failure(code: &'static str, message: &str, request_id: Option<String>) -> Result<()> {
    render_outcome(
        failure(code, message, PrContext::default()),
        true,
        request_id,
    )
}

fn effect(
    action: &str,
    attempted: bool,
    completed: bool,
    details: Option<Value>,
) -> crate::protocol::Effect {
    crate::protocol::Effect {
        action: action.into(),
        attempted,
        completed,
        details,
    }
}

fn diagnostic(source: &str, content: &str) -> Vec<crate::protocol::Diagnostic> {
    const LIMIT: usize = 64 * 1024;
    let bytes = content.as_bytes();
    if bytes.is_empty() {
        return Vec::new();
    }
    vec![crate::protocol::Diagnostic {
        source: source.into(),
        stream: "stderr".into(),
        content: String::from_utf8_lossy(&bytes[..bytes.len().min(LIMIT)]).into_owned(),
        original_size: bytes.len(),
        truncated: bytes.len() > LIMIT,
    }]
}

fn retry_action() -> crate::protocol::RecoveryAction<Value> {
    crate::protocol::RecoveryAction {
        action: "retry".into(),
        description: "Fix the failed publication step, then retry PR creation. Do not force-push."
            .into(),
        mutation: crate::protocol::MutationClass::None,
        requires_human_approval: true,
        invocation: crate::protocol::RecoveryInvocation {
            argv: vec!["pando".into(), "pr".into(), "create".into()],
            stdin: None,
            working_directory: None,
        },
    }
}

#[allow(clippy::unnecessary_wraps)]
fn fail_dirty(_json_mode: bool) -> Result<PrOutcome> {
    Ok(PrOutcome {
        result: Err(PrFailure {
            code: "repository.dirty",
            message: "topic worktree is dirty; commit changes first or retry with --yolo".into(),
        }),
        context: PrContext {
            dirty: Some(true),
            ..PrContext::default()
        },
        effects: Vec::new(),
        diagnostics: Vec::new(),
        recovery: vec![retry_action()],
    })
}

fn failure(code: &'static str, message: &str, context: PrContext) -> PrOutcome {
    PrOutcome {
        result: Err(PrFailure {
            code,
            message: message.into(),
        }),
        context,
        effects: Vec::new(),
        diagnostics: Vec::new(),
        recovery: Vec::new(),
    }
}

#[allow(clippy::unnecessary_wraps)]
fn fail(_json_mode: bool, code: &'static str, message: &str) -> Result<PrOutcome> {
    Ok(failure(code, message, PrContext::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_defaults_to_draft() {
        let request: Request = serde_json::from_str(r#"{"title":"T","description":"B"}"#).unwrap();
        assert_eq!(request.status, Status::Draft);
    }

    #[test]
    fn request_rejects_unknown_fields() {
        assert!(
            serde_json::from_str::<Request>(r#"{"title":"T","description":"B","force":true}"#)
                .is_err()
        );
    }

    #[test]
    fn metadata_requires_nonempty_title_and_description() {
        assert!(parse_metadata("# \nbody").is_err());
        assert!(parse_metadata("# title\n").is_err());
        assert_eq!(
            parse_metadata("# title\nbody").unwrap(),
            ("title".into(), "body".into())
        );
    }

    #[test]
    fn generation_retries_a_rejected_document_up_to_three_times() {
        let mut prompts = Vec::new();
        let metadata = generate_with_retries("PROMPT", |prompt| {
            prompts.push(prompt.to_owned());
            if prompts.len() < 3 {
                parse_metadata("Here is the PR:\n# title\nbody")
            } else {
                parse_metadata("# title\nbody")
            }
        })
        .unwrap();

        assert_eq!(metadata, ("title".into(), "body".into()));
        assert_eq!(prompts.len(), 3);
        assert_eq!(prompts[0], "PROMPT");
        for prompt in &prompts[1..] {
            assert!(prompt.starts_with("PROMPT"), "{prompt}");
            assert!(
                prompt.contains("level-one heading"),
                "retry prompt should report why the last document was rejected: {prompt}"
            );
        }
    }

    #[test]
    fn generation_gives_up_after_three_failures() {
        let mut attempts = 0;
        let error = generate_with_retries("PROMPT", |_| {
            attempts += 1;
            bail!("PR metadata generator failed")
        })
        .unwrap_err();

        assert_eq!(attempts, 3);
        assert!(
            format!("{error:#}").contains("failed after 3 attempts"),
            "{error:#}"
        );
        assert!(
            format!("{error:#}").contains("PR metadata generator failed"),
            "{error:#}"
        );
    }

    #[test]
    fn generation_does_not_retry_a_document_that_parses() {
        let mut attempts = 0;
        generate_with_retries("PROMPT", |_| {
            attempts += 1;
            parse_metadata("# title\nbody")
        })
        .unwrap();
        assert_eq!(attempts, 1);
    }

    #[test]
    fn metadata_tolerates_leading_blank_lines_and_a_wrapping_fence() {
        assert_eq!(
            parse_metadata("\n\n# title\nbody\n").unwrap(),
            ("title".into(), "body".into())
        );
        assert_eq!(
            parse_metadata("```markdown\n# title\nbody\n```\n").unwrap(),
            ("title".into(), "body".into())
        );
        assert_eq!(
            parse_metadata("```\n# title\nbody\n```").unwrap(),
            ("title".into(), "body".into())
        );
    }

    #[test]
    fn metadata_preserves_a_fence_inside_the_description() {
        assert_eq!(
            parse_metadata("# title\nbody\n\n```sh\nls\n```").unwrap(),
            ("title".into(), "body\n\n```sh\nls\n```".into())
        );
        // An unterminated leading fence is not a wrapper, so it still fails.
        assert!(parse_metadata("```markdown\n# title\nbody").is_err());
    }

    #[test]
    fn metadata_still_rejects_a_missing_level_one_heading() {
        assert!(parse_metadata("## title\nbody").is_err());
        assert!(parse_metadata("Here is the PR:\n# title\nbody").is_err());
    }

    #[test]
    fn request_rejects_description_conflict() {
        let request: Request =
            serde_json::from_str(r#"{"description":"inline","description_file":"body.md"}"#)
                .unwrap();
        assert!(request.description.is_some() && request.description_file.is_some());
    }
}
