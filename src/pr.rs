use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    env, fs,
    io::{self, IsTerminal, Read},
    process::Command,
};

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
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub description_file: Option<String>,
    #[serde(default)]
    pub status: Status,
    #[serde(default)]
    pub dry_run: bool,
}
#[allow(clippy::struct_excessive_bools)]
pub struct Invocation {
    pub title: Option<String>,
    pub description: Option<String>,
    pub description_file: Option<String>,
    pub status: Status,
    pub dry_run: bool,
    pub force: bool,
    pub json: bool,
    pub request_mode: bool,
}
/// Creates a pull request after validating repository and provider state.
///
/// # Errors
/// Returns an error when validation, provider preflight, or creation fails.
pub fn run(inv: Invocation) -> Result<()> {
    if inv.request_mode {
        if inv.title.is_some()
            || inv.description.is_some()
            || inv.description_file.is_some()
            || inv.force
        {
            bail!("json.invalid_request: command options are forbidden with --input-output json");
        }
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        let r: Request = serde_json::from_str(&input).context("invalid JSON request")?;
        if r.description_file.as_deref() == Some("-") {
            bail!("stdin is not allowed as a description-file source");
        }
        return execute(
            r.title,
            body(r.description, r.description_file)?,
            r.status,
            r.dry_run,
            false,
            true,
        );
    }
    let title = inv.title.context("pr create requires --title")?;
    if inv.description.is_some() && inv.description_file.is_some() {
        bail!("--description conflicts with --description-file");
    }
    execute(
        title,
        body(inv.description, inv.description_file)?,
        inv.status,
        inv.dry_run,
        inv.force,
        inv.json,
    )
}
fn body(desc: Option<String>, file: Option<String>) -> Result<String> {
    if let Some(f) = file {
        if f == "-" {
            let mut s = String::new();
            io::stdin().read_to_string(&mut s)?;
            Ok(s)
        } else {
            Ok(fs::read_to_string(f)?)
        }
    } else {
        desc.context("a pull request description is required")
    }
}
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
fn execute(
    title: String,
    body: String,
    status: Status,
    dry: bool,
    force: bool,
    json_mode: bool,
) -> Result<()> {
    let cwd = env::current_dir()?;
    let repo = crate::git::repository(&cwd)?;
    let base = crate::config::EffectiveConfig::load(&repo)?
        .require_target_branch()?
        .to_owned();
    let head = crate::git::current_branch(&repo)?.to_owned();
    if head == base {
        return fail(
            json_mode,
            "pr.invalid_source",
            "current branch is the configured target branch",
        );
    }
    if crate::git::is_dirty(&repo.current().path)? {
        return fail(
            json_mode,
            "repository.dirty",
            "topic worktree must be clean",
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
    if crate::git::remote_matches(&repo.current().path, &head)?.is_empty() {
        return fail(
            json_mode,
            "pr.unpublished",
            "topic branch is not published to a same-repository remote",
        );
    }
    let existing = Command::new("gh")
        .args([
            "pr", "list", "--head", &head, "--base", &base, "--state", "open", "--json", "url",
        ])
        .output()?;
    if !existing.status.success() {
        return fail(
            json_mode,
            "provider.preflight_failed",
            "GitHub pull request preflight failed",
        );
    }
    let urls: Vec<serde_json::Value> = serde_json::from_slice(&existing.stdout).unwrap_or_default();
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
    if dry {
        return output(
            json_mode,
            json!({"outcome":"dry_run","base":base,"head":head,"draft":status==Status::Draft}),
            None,
        );
    }
    let mut cmd = Command::new("gh");
    cmd.args([
        "pr", "create", "--base", &base, "--head", &head, "--title", &title, "--body", &body,
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
        json!({"outcome":"created","url":url,"base":base,"head":head,"draft":status==Status::Draft}),
        Some(url),
    )
}
fn output(j: bool, r: serde_json::Value, h: Option<String>) -> Result<()> {
    if j {
        crate::protocol::write(&crate::protocol::success(
            "pr.create",
            None,
            r,
            json!({}),
            vec![],
        ))?;
    } else {
        crate::ui::finish(
            crate::ui::success_style()
                .apply_to(h.unwrap_or_else(|| "Pull request dry-run complete.".to_owned())),
        )?;
    }
    Ok(())
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
}
