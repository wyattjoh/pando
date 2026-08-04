//! Collapsing a topic branch into one commit as part of the merge lifecycle.
//!
//! Squashing runs *after* any rebase and *before* the fast-forward merge, so by
//! the time it starts the target is already an ancestor of the topic and the
//! collapse is a `reset --soft` back to the target followed by one commit. That
//! ordering is why this module never resolves a merge base: `lifecycle` has
//! already guaranteed the linear relationship it depends on.

use std::{
    io::Write,
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};
use minijinja::{Environment, context};

use crate::{
    WorktreeKind,
    config::{EffectiveConfig, EffectiveGeneration, GenerationSource},
    git::{self, LifecycleMutation, Repository},
    trust,
};

const BUILTIN_TEMPLATE: &str = r"Write a factual conventional commit message describing this branch as a single change.
Return only the commit message.
The subject must use imperative mood and be fewer than 50 characters.
Follow it with a blank line and at least two concrete bullet items.
Summarize the branch as one coherent change rather than narrating its {{ commit_count }} commits.
Do not claim changes not evidenced by the diff.

Repository: {{ repo }}
Branch: {{ branch }}
Merging into: {{ target }}

Messages of the {{ commit_count }} commits being squashed:
{% for message in commits %}---
{{ message }}
{% endfor %}---

Diffstat against {{ target }}:
{{ git_diff_stat }}

Diff against {{ target }}:
{{ git_diff }}
";

/// What squashing would do to the topic, decided before anything mutates.
#[derive(Clone, Copy, Debug)]
pub struct SquashPlan {
    /// Squashing is enabled and there is more than one commit to collapse.
    pub applicable: bool,
    /// Commits currently between the target and the topic's `HEAD`.
    pub commit_count: usize,
    /// A generator command is resolvable for the squash message.
    pub generator_configured: bool,
    /// Every shared generator value is approved for this clone.
    pub generator_trusted: bool,
}

impl SquashPlan {
    /// A plan that runs no squash, used whenever the policy or phase rules it out.
    #[must_use]
    pub const fn skipped() -> Self {
        Self {
            applicable: false,
            commit_count: 0,
            generator_configured: true,
            generator_trusted: true,
        }
    }

    /// Reports whether the plan is blocked on generator approval.
    #[must_use]
    pub const fn approval_required(&self) -> bool {
        self.applicable && !self.generator_trusted
    }
}

/// Decides whether the topic will be squashed, without mutating anything.
///
/// Pass `countable = false` while a rebase is still in flight: `HEAD` then
/// points at a partially replayed branch, so the range against `target` would
/// describe a history that is about to change. The plan stays applicable, it
/// just declines to claim a count it cannot yet trust.
///
/// # Errors
///
/// Returns an error when Git cannot walk the range or trust cannot be read.
pub fn plan(
    repository: &Repository,
    config: &EffectiveConfig,
    target: &str,
    enabled: bool,
    countable: bool,
    include_staged: bool,
) -> Result<SquashPlan> {
    if !enabled || !config.squash {
        return Ok(SquashPlan::skipped());
    }
    let commit_count = if countable {
        let head = git::head_commit(&repository.current().path)?;
        let count = git::count_commits_between(&repository.current().path, target, &head)?;
        // One commit is already the shape a squash produces, unless staged
        // yolo changes must be folded into a newly generated message.
        if count < 2 && !include_staged {
            return Ok(SquashPlan {
                commit_count: count,
                ..SquashPlan::skipped()
            });
        }
        count
    } else {
        0
    };
    let generation = &config.merge_generation;
    Ok(SquashPlan {
        applicable: true,
        commit_count,
        generator_configured: generation.command.is_some(),
        generator_trusted: !is_shared(generation)
            || trust::is_merge_generation_trusted(repository, generation)?,
    })
}

/// Fails unless a planned squash could actually run to completion.
///
/// Callers use this as a preflight so a missing or untrusted generator is
/// reported before the lifecycle mutates anything.
///
/// # Errors
///
/// Returns an error when the squash applies but its generator is unconfigured
/// or awaiting approval.
pub fn ensure_ready(
    repository: &Repository,
    config: &EffectiveConfig,
    target: &str,
    countable: bool,
    include_staged: bool,
) -> Result<()> {
    let plan = plan(repository, config, target, true, countable, include_staged)?;
    if !plan.applicable {
        return Ok(());
    }
    if !plan.generator_configured {
        bail!(
            "no squash message generator is configured; set merge.generation.command or commit.generation.command, or rerun with --no-squash"
        );
    }
    if !plan.generator_trusted {
        bail!(
            "shared squash message generator approval is required; run pando trust merge-approve, or rerun with --no-squash"
        );
    }
    Ok(())
}

/// Collapses the topic onto `target` under `message`.
///
/// Generation is a separate step ([`generate_message`]) so the caller can show
/// the message it is about to commit before the history is rewritten.
///
/// Git's transcript is deliberately discarded on success: the caller has
/// already rendered the message, and the fast-forward that follows reports the
/// same diffstat. Failures still carry it, because `git` output is folded into
/// the error.
///
/// # Errors
///
/// Returns an error when Git cannot rewrite the branch.
pub fn collapse(repository: &Repository, target: &str, message: &str) -> Result<()> {
    let cwd = &repository.current().path;
    LifecycleMutation::new(cwd).reset_soft(target)?;
    LifecycleMutation::new(cwd).commit_message(message)?;
    Ok(())
}

/// Renders the squash prompt and runs the configured generator.
///
/// The approval check is repeated here rather than left to the caller's
/// preflight: this is where the configured command actually executes, so no
/// future caller can reach it without having cleared trust.
///
/// # Errors
///
/// Returns an error when the generator is missing, untrusted, or fails, or
/// when its output is not a usable message.
///
/// # Panics
///
/// Panics if the child's piped stdin is unavailable, which cannot happen for a
/// process spawned with `Stdio::piped`.
pub fn generate_message(
    repository: &Repository,
    config: &EffectiveConfig,
    target: &str,
    include_staged: bool,
) -> Result<String> {
    let generation = &config.merge_generation;
    if is_shared(generation) && !trust::is_merge_generation_trusted(repository, generation)? {
        bail!(
            "shared squash message generator approval is required; run pando trust merge-approve, or rerun with --no-squash"
        );
    }
    let command = &config
        .merge_generation
        .command
        .as_ref()
        .context(
            "no squash message generator is configured; set merge.generation.command or commit.generation.command, or rerun with --no-squash",
        )?
        .value;
    let template = config
        .merge_generation
        .template
        .as_ref()
        .map_or(BUILTIN_TEMPLATE, |value| value.value.as_str());
    let prompt = render_prompt(repository, template, target, include_staged)?;
    let mut child = Command::new("/bin/sh")
        .args(["-c", command])
        .current_dir(&repository.current().path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to start the squash message generator")?;
    if let Err(error) = child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(prompt.as_bytes())
        && error.kind() != std::io::ErrorKind::BrokenPipe
    {
        return Err(error).context("failed to send the squash prompt to the generator");
    }
    let output = child
        .wait_with_output()
        .context("failed to await the squash message generator")?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.trim();
        if detail.is_empty() {
            bail!(
                "squash message generator failed with status {}",
                output.status
            );
        }
        bail!(
            "squash message generator failed with status {}\n{detail}",
            output.status
        );
    }
    let message = String::from_utf8(output.stdout)
        .context("squash message generator produced non-UTF-8 output")?
        .trim()
        .to_owned();
    if message.is_empty() {
        bail!("squash message generator produced an empty message");
    }
    Ok(message)
}

fn render_prompt(
    repository: &Repository,
    template: &str,
    target: &str,
    include_staged: bool,
) -> Result<String> {
    let mut environment = Environment::new();
    environment
        .add_template("squash", template)
        .context("failed to parse the squash generation template")?;
    let cwd = &repository.current().path;
    let head = git::head_commit(cwd)?;
    let diff_source = if include_staged {
        git::RangeDiffSource::Staged
    } else {
        git::RangeDiffSource::Committed
    };
    let range = git::HistoryObservation::new(cwd).range(target, &head, diff_source)?;
    let branch = match &repository.current().kind {
        WorktreeKind::Branch(value) => value.as_str(),
        WorktreeKind::Detached => "(detached)",
        _ => "(unknown)",
    };
    let repo = cwd.file_name().map_or_else(
        || "(unknown)".into(),
        |value| value.to_string_lossy().into_owned(),
    );
    environment
        .get_template("squash")?
        .render(context! {
            branch,
            target,
            repo,
            commit_count => range.commit_count,
            commits => range.messages,
            git_diff => range.patch,
            git_diff_stat => range.statistics,
        })
        .context("failed to render the squash generation template")
}

/// Reports whether any effective generator value came from committed configuration.
fn is_shared(generation: &EffectiveGeneration) -> bool {
    [generation.command.as_ref(), generation.template.as_ref()]
        .into_iter()
        .flatten()
        .any(|value| value.source == GenerationSource::Shared)
}

#[cfg(test)]
mod tests {
    use super::BUILTIN_TEMPLATE;
    use minijinja::Environment;

    #[test]
    fn builtin_template_parses() {
        let mut environment = Environment::new();
        environment
            .add_template("squash", BUILTIN_TEMPLATE)
            .unwrap();
    }
}
