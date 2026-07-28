use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use minijinja::{Environment, context};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    env, fs,
    io::{self, IsTerminal, Read, Write},
    process::Command,
};

use cliclack::confirm;
use console::{Key, Term};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    #[default]
    Draft,
    Ready,
}
#[derive(Debug, Deserialize, Serialize)]
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
        return execute(
            inv.title,
            body_optional(inv.description, inv.description_file)?,
            Status::Ready,
            false,
            true,
            true,
            false,
            inv.remote,
        );
    }
    if inv.request_mode {
        if inv.title.is_some() || inv.description.is_some() || inv.description_file.is_some() {
            bail!("json.invalid_request: command options are forbidden with --input-output json");
        }
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        let r: Request = serde_json::from_str(&input).context("invalid JSON request")?;
        if r.description_file.as_deref() == Some("-") {
            bail!("stdin is not allowed as a description-file source");
        }
        if r.description.is_some() && r.description_file.is_some() {
            bail!("json.invalid_request: description conflicts with description_file");
        }
        return execute(
            r.title,
            body_optional(r.description, r.description_file)?,
            r.status,
            r.dry_run,
            inv.force,
            false,
            true,
            r.remote,
        );
    }
    let title = inv.title;
    if inv.description.is_some() && inv.description_file.is_some() {
        bail!("--description conflicts with --description-file");
    }
    execute(
        title,
        body_optional(inv.description, inv.description_file)?,
        inv.status,
        inv.dry_run,
        inv.force,
        false,
        inv.json,
        inv.remote,
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
) -> Result<()> {
    let cwd = env::current_dir()?;
    let repo = crate::git::repository(&cwd)?;
    let config = crate::config::EffectiveConfig::load(&repo)?;
    let base =
        crate::git::resolve_target_branch(&repo.current().path, config.target_branch.as_deref())?;
    let head = crate::git::current_branch(&repo)?.to_owned();
    let base_remote = crate::git::branch_upstream_remote(&repo.current().path, &base)?
        .context("target branch has no upstream; cannot resolve base repository")?;
    let base_repo = github_repository(&crate::git::remote_url(&repo.current().path, &base_remote)?)
        .context("configured target upstream is not a supported GitHub remote")?;
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
    let head_repository = github_repository(&crate::git::remote_url(
        &repo.current().path,
        &push_plan.remote,
    )?)
    .context("selected head remote is not a supported GitHub remote")?;
    let head_owner = head_repository
        .split('/')
        .next()
        .unwrap_or_default()
        .to_owned();
    // Resolve dirty state before generating metadata so skipped or committed changes are
    // represented by the committed range sent to the generator.
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
    if title.is_none() || body.is_none() {
        let config = crate::config::EffectiveConfig::load(&repo)?;
        let generator = config
            .pr_generation
            .command
            .as_ref()
            .context("missing PR metadata generator configuration")?;
        if generator.source == crate::config::GenerationSource::Shared
            && !crate::trust::is_pr_generation_trusted(&repo, &config.pr_generation)?
        {
            return fail(
                json_mode,
                "trust.approval_required",
                "shared PR generator approval is required; run worktrees trust pr-approve",
            );
        }
        if !dry {
            let pull_request_template = resolved_pull_request_template(&repo, &config)?;
            let repo_name = repo
                .current()
                .path
                .file_name()
                .map_or("(unknown)".into(), |v| v.to_string_lossy().into_owned());
            let diffstat = git_cmd(&repo, &["diff", "--stat", &format!("{base}...HEAD")])?;
            let diff = git_cmd(&repo, &["diff", &format!("{base}...HEAD")])?;
            let subjects = git_cmd(&repo, &["log", "--format=%s", &format!("{base}..HEAD")])?;
            let explicit_title = title.as_deref().unwrap_or("");
            let explicit_description = body.as_deref().unwrap_or("");
            let prompt = if let Some(template) = config.pr_generation.template.as_ref() {
                let mut environment = Environment::new();
                environment.add_template("pr", &template.value)?;
                environment.get_template("pr")?.render(context! {
                    repo => repo_name, branch => head, base, git_diff_stat => diffstat,
                    git_diff => diff, git_commit_subjects => subjects,
                    explicit_title => title.as_deref().unwrap_or(""),
                    explicit_description => body.as_deref().unwrap_or(""), pull_request_template
                })?
            } else {
                format!(
                    "Generate a PR metadata document. Return exactly one first-line level-one heading, followed by the description. Preserve required headings, checklists, and sections from the pull-request template; replace placeholders and instructional comments with factual content.\nRepository: {repo_name}\nTopic branch: {head}\nTarget branch: {base}\nDiffstat:\n{diffstat}\nCommitted commit subjects:\n{subjects}\nExplicit title: {explicit_title}\nExplicit description:\n{explicit_description}\nDiff:\n{diff}\nPull-request template:\n{pull_request_template}\n"
                )
            };
            let mut child = Command::new("/bin/sh")
                .args(["-c", &generator.value])
                .current_dir(&repo.current().path)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()?;
            child
                .stdin
                .take()
                .context("generator stdin unavailable")?
                .write_all(prompt.as_bytes())?;
            let out = child.wait_with_output()?;
            if !out.status.success() {
                return fail(
                    json_mode,
                    "pr.generator_failed",
                    "PR metadata generator failed",
                );
            }
            let (generated_title, generated_body) = parse_metadata(
                &String::from_utf8(out.stdout)
                    .map_err(|_| anyhow::anyhow!("PR generator produced non-UTF-8 output"))?,
            )?;
            if title.is_none() {
                title = Some(generated_title);
            }
            if body.is_none() {
                body = Some(generated_body);
            }
        }
    }
    let mut title = title.context("PR title is required")?;
    let mut body = body.unwrap_or_default();
    if title.trim().is_empty() {
        bail!("PR title cannot be empty");
    }
    if head == base {
        return fail(
            json_mode,
            "pr.invalid_source",
            "current branch is the configured target branch",
        );
    }
    if !force && !io::stdout().is_terminal() && !json_mode {
        return fail(
            json_mode,
            "pr.approval_required",
            "non-interactive creation requires --force",
        );
    }
    let gh = Command::new("gh")
        .arg("--version")
        .output()
        .map_err(|_| anyhow::anyhow!("gh is not installed; install GitHub CLI"))?;
    if !gh.status.success() {
        return fail(
            json_mode,
            "provider.unauthenticated",
            "gh is unavailable; install GitHub CLI and run gh auth login",
        );
    }
    if !Command::new("gh")
        .args(["auth", "status"])
        .output()?
        .status
        .success()
    {
        return fail(
            json_mode,
            "provider.unauthenticated",
            "GitHub CLI is not authenticated; run gh auth login",
        );
    }
    let head_ref = github_head_ref(&base_repo, &head_repository, &head_owner, &head);
    let existing = Command::new("gh")
        .args([
            "pr", "list", "--repo", &base_repo, "--head", &head_ref, "--base", &base, "--state",
            "open", "--json", "url",
        ])
        .output()?;
    if !existing.status.success() {
        return fail(
            json_mode,
            "provider.preflight_failed",
            "GitHub pull request preflight failed",
        );
    }
    let urls: Vec<serde_json::Value> = serde_json::from_slice(&existing.stdout)
        .context("GitHub pull request preflight returned malformed JSON")?;
    if let Some(url) = urls
        .first()
        .and_then(|v| v.get("url"))
        .and_then(|v| v.as_str())
    {
        return fail(
            json_mode,
            "pr.already_exists",
            &format!("an open pull request already exists: {url}"),
        );
    }
    let dirty = crate::git::is_dirty(&repo.current().path)?;
    if dirty {
        if force && !yolo {
            bail!(
                "repository.dirty: topic worktree is dirty; commit changes first or retry with --yolo"
            );
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
    if !force && !dry {
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
        return output(
            json_mode,
            json!({"outcome":"dry_run","base_repository":base_repo,"base_branch":base,"head_repository":format!("{head_owner}"),"head_branch":head,"remote":push_plan.remote,"draft":status==Status::Draft,"push":push_effect}),
            None,
            None,
        );
    }
    if let Err(error) = crate::git::push(&repo.current().path, &push_plan, !json_mode) {
        if json_mode {
            crate::protocol::write(&crate::protocol::Response {
                schema_version: crate::protocol::SCHEMA_VERSION,
                request_id: None,
                command: "pr.create".into(), status: "error", result: None,
                error: Some(crate::protocol::ErrorBody { code: "git.push_failed".into(), message: format!("{error:#}") }),
                context: json!({"base":base,"head":head}),
                effects: vec![crate::protocol::Effect { action: "git.push".into(), attempted: true, completed: false, details: Some(push_effect) }],
                diagnostics: vec![], next_steps: vec![crate::protocol::NextStep { action: "retry".into(), description: "Fix the remote or branch divergence, then retry PR creation. Do not force-push.".into(), mutation: "none".into(), requires_human_approval: true, invocation: json!({"command":"worktrees pr create"}) }],
            })?;
            return Ok(());
        }
        return fail(false, "git.push_failed", &format!("{error:#}"));
    }
    let mut cmd = Command::new("gh");
    cmd.args([
        "pr", "create", "--repo", &base_repo, "--base", &base, "--head", &head_ref, "--title",
        &title, "--body", &body,
    ]);
    if status == Status::Draft {
        cmd.arg("--draft");
    }
    let out = cmd.output()?;
    if !out.status.success() {
        return fail(
            json_mode,
            "provider.creation_failed",
            &String::from_utf8_lossy(&out.stderr),
        );
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    output(
        json_mode,
        json!({"outcome":"created","url":url,"base_repository":base_repo,"base_branch":base,"head_repository":head_owner,"head_branch":head,"remote":push_plan.remote,"draft":status==Status::Draft}),
        Some(url),
        Some(crate::protocol::Effect {
            action: "git.push".into(),
            attempted: true,
            completed: true,
            details: Some(push_effect),
        }),
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
        crate::ui::info(crate::ui::heading_style().apply_to("Review pull request"))?;
        crate::ui::step(format!("base: {base_repo} ({base})"))?;
        crate::ui::step(format!("head: {head_repo} ({head})"))?;
        crate::ui::step(format!("push remote: {remote}"))?;
        crate::ui::step(format!(
            "status: {}",
            if status == Status::Draft {
                "draft"
            } else {
                "ready"
            }
        ))?;
        crate::ui::step(crate::ui::heading_style().apply_to(format!("# {title}")))?;
        render_markdown(&body)?;
        crate::ui::info("Press Enter to create, Ctrl-G to edit, or Escape to cancel.")?;
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
                let path = env::temp_dir().join(format!("worktrees-pr-{}.md", std::process::id()));
                fs::write(&path, format!("# {title}\n\n{body}\n"))?;
                let editor = resolve_editor()?;
                let status = Command::new("/bin/sh")
                    .args(["-c", &format!(r#"{editor} "$1""#)])
                    .arg("worktrees-pr-editor")
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

fn render_markdown(body: &str) -> Result<()> {
    for line in body.lines() {
        let rendered = if line.starts_with('#') || line.starts_with("```") {
            crate::ui::heading_style().apply_to(line).to_string()
        } else if line.starts_with("- ") || line.starts_with("* ") {
            crate::ui::worktree_data_style().apply_to(line).to_string()
        } else {
            line.to_owned()
        };
        crate::ui::info(rendered)?;
    }
    Ok(())
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

fn parse_metadata(value: &str) -> Result<(String, String)> {
    let mut lines = value.lines();
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

fn github_head_ref(base_repo: &str, head_repo: &str, head_owner: &str, head: &str) -> String {
    if base_repo == head_repo {
        head.to_owned()
    } else {
        format!("{head_owner}:{head}")
    }
}

fn github_repository(url: &str) -> Option<String> {
    let value = url.trim_end_matches('/').trim_end_matches(".git");
    let path = value
        .strip_prefix("https://github.com/")
        .or_else(|| value.strip_prefix("http://github.com/"))
        .or_else(|| value.strip_prefix("git@github.com:"))
        .or_else(|| value.strip_prefix("ssh://git@github.com/"))?;
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    (parts.next().is_none() && !owner.is_empty() && !repo.is_empty())
        .then(|| format!("{owner}/{repo}"))
}

fn output(
    j: bool,
    r: serde_json::Value,
    h: Option<String>,
    effect: Option<crate::protocol::Effect>,
) -> Result<()> {
    if j {
        crate::protocol::write(&crate::protocol::success(
            "pr.create",
            None,
            r,
            json!({}),
            effect.into_iter().collect(),
        ))?;
    } else {
        crate::ui::finish(
            crate::ui::success_style()
                .apply_to(h.unwrap_or_else(|| "Pull request dry-run complete.".to_owned())),
        )?;
    }
    Ok(())
}
fn fail_dirty(json_mode: bool) -> Result<()> {
    let message = "topic worktree is dirty; commit changes first or retry with --yolo";
    if json_mode {
        crate::protocol::write(&crate::protocol::Response {
            schema_version: crate::protocol::SCHEMA_VERSION,
            request_id: None,
            command: "pr.create".into(),
            status: "error",
            result: None,
            error: Some(crate::protocol::ErrorBody {
                code: "repository.dirty".into(),
                message: message.into(),
            }),
            context: json!({"dirty": true}),
            effects: vec![],
            diagnostics: vec![],
            next_steps: vec![crate::protocol::NextStep {
                action: "retry".into(),
                description:
                    "Commit the changes, or retry with --yolo to commit them automatically.".into(),
                mutation: "none".into(),
                requires_human_approval: true,
                invocation: json!({"command": "worktrees pr create --yolo"}),
            }],
        })?;
        return Ok(());
    }
    bail!("repository.dirty: {message}")
}

fn fail(j: bool, c: &str, m: &str) -> Result<()> {
    if j {
        crate::protocol::write(&crate::protocol::failure("pr.create", None, c, m))?;
        Ok(())
    } else {
        bail!("{c}: {m}")
    }
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
    fn github_head_ref_uses_branch_for_same_repository() {
        assert_eq!(
            github_head_ref("alice/project", "alice/project", "alice", "feature"),
            "feature"
        );
    }

    #[test]
    fn github_head_ref_qualifies_fork_branch() {
        assert_eq!(
            github_head_ref("alice/project", "bob/project", "bob", "feature"),
            "bob:feature"
        );
    }

    #[test]
    fn github_repository_accepts_supported_remote_forms() {
        assert_eq!(
            github_repository("git@github.com:alice/project.git"),
            Some("alice/project".into())
        );
        assert_eq!(
            github_repository("https://github.com/alice/project"),
            Some("alice/project".into())
        );
        assert_eq!(github_repository("https://gitlab.com/alice/project"), None);
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
    fn request_rejects_description_conflict() {
        let request: Request =
            serde_json::from_str(r#"{"description":"inline","description_file":"body.md"}"#)
                .unwrap();
        assert!(request.description.is_some() && request.description_file.is_some());
    }
}
