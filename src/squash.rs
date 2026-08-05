//! Opaque preparation and collapse of a topic branch into one commit.
//!
//! Squashing runs after rebase and before validation. The capsule binds the
//! generated message to observed topic, target, index, and worktree facts so
//! lifecycle can durably checkpoint preparation without owning Git mutation.

use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};
use minijinja::{Environment, context};

use crate::{
    WorktreeKind,
    config::{EffectiveConfig, EffectiveGeneration, GenerationSource},
    git::{
        HistoryObservation, LifecycleMutation, RangeDiffSource, Repository, RepositoryObservation,
    },
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

/// Read-only inputs to squash assessment.
#[derive(Clone, Copy)]
pub(crate) struct AssessRequest<'a> {
    pub(crate) repository: &'a Repository,
    pub(crate) config: &'a EffectiveConfig,
    pub(crate) target: &'a str,
    pub(crate) enabled: bool,
    pub(crate) final_history: bool,
    pub(crate) include_staged: bool,
}

/// Why assessment did not produce a squash capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SkipReason {
    Disabled,
    SingleCommit,
}

/// A pre-mutation squash blocker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BlockReason {
    GeneratorMissing,
    ApprovalRequired,
}

/// Read-only squash assessment. The required value is opaque to callers.
#[derive(Debug)]
pub(crate) enum Assessment {
    Skipped {
        reason: SkipReason,
        commit_count: usize,
    },
    Blocked {
        reason: BlockReason,
        commit_count: usize,
    },
    PendingFinalHistory,
    Required(RequiredSquash),
}

impl Assessment {
    #[must_use]
    pub(crate) const fn applicable(&self) -> bool {
        matches!(
            self,
            Self::Blocked { .. } | Self::PendingFinalHistory | Self::Required(_)
        )
    }

    #[must_use]
    pub(crate) const fn commit_count(&self) -> usize {
        match self {
            Self::Skipped { commit_count, .. } | Self::Blocked { commit_count, .. } => {
                *commit_count
            }
            Self::PendingFinalHistory => 0,
            Self::Required(required) => required.commit_count,
        }
    }

    #[must_use]
    pub(crate) const fn generator_configured(&self) -> bool {
        !matches!(
            self,
            Self::Blocked {
                reason: BlockReason::GeneratorMissing,
                ..
            }
        )
    }

    #[must_use]
    pub(crate) const fn generator_trusted(&self) -> bool {
        !matches!(
            self,
            Self::Blocked {
                reason: BlockReason::ApprovalRequired,
                ..
            }
        )
    }

    #[must_use]
    pub(crate) const fn approval_required(&self) -> bool {
        matches!(
            self,
            Self::Blocked {
                reason: BlockReason::ApprovalRequired,
                ..
            }
        )
    }

    pub(crate) fn into_required(self) -> Result<RequiredSquash> {
        match self {
            Self::Required(required) => Ok(required),
            Self::Skipped {
                reason: SkipReason::Disabled,
                ..
            } => bail!("squashing is disabled"),
            Self::Skipped {
                reason: SkipReason::SingleCommit,
                ..
            } => bail!("the topic already contains a single commit"),
            Self::Blocked {
                reason: BlockReason::GeneratorMissing,
                ..
            } => bail!(missing_generator_message()),
            Self::Blocked {
                reason: BlockReason::ApprovalRequired,
                ..
            } => bail!(approval_message()),
            Self::PendingFinalHistory => {
                bail!("final post-rebase history is required before squash preparation")
            }
        }
    }
}

/// Opaque authority to perform final squash preparation.
#[derive(Debug)]
pub(crate) struct RequiredSquash {
    topic_worktree: PathBuf,
    topic_branch: String,
    target_branch: String,
    commit_count: usize,
    include_staged: bool,
    generation: EffectiveGeneration,
}

/// A final-history preparation result.
pub(crate) enum Preparation {
    Skipped,
    Prepared(PreparedSquash),
}

/// Opaque prepared state consumed by [`collapse`].
pub(crate) struct PreparedSquash {
    checkpoint: PreparedCheckpoint,
}

impl PreparedSquash {
    #[must_use]
    pub(crate) fn message(&self) -> &str {
        &self.checkpoint.message
    }

    #[must_use]
    pub(crate) fn commit_count(&self) -> usize {
        self.checkpoint.commit_count
    }

    #[must_use]
    pub(crate) fn checkpoint(&self) -> PreparedCheckpoint {
        self.checkpoint.clone()
    }
}

/// Semantic evidence persisted before generated-message presentation or reset.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreparedCheckpoint {
    topic_worktree: PathBuf,
    topic_branch: String,
    target_branch: String,
    expected_topic_commit: String,
    expected_target_commit: String,
    expected_result_tree: String,
    commit_count: usize,
    include_staged: bool,
    message: String,
}

impl PreparedCheckpoint {
    pub(crate) fn topic_worktree(&self) -> &Path {
        &self.topic_worktree
    }
    pub(crate) fn topic_branch(&self) -> &str {
        &self.topic_branch
    }
    pub(crate) fn target_branch(&self) -> &str {
        &self.target_branch
    }
    pub(crate) fn expected_topic_commit(&self) -> &str {
        &self.expected_topic_commit
    }
    pub(crate) fn expected_target_commit(&self) -> &str {
        &self.expected_target_commit
    }
    pub(crate) fn expected_result_tree(&self) -> &str {
        &self.expected_result_tree
    }
    pub(crate) const fn commit_count(&self) -> usize {
        self.commit_count
    }
    pub(crate) const fn include_staged(&self) -> bool {
        self.include_staged
    }
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

/// Verified result of a collapse.
pub(crate) struct CollapsedSquash {
    commit: String,
}

impl CollapsedSquash {
    #[must_use]
    pub(crate) fn commit(&self) -> &str {
        &self.commit
    }
}

/// Assesses applicability and generator readiness without mutation.
pub(crate) fn assess(request: AssessRequest<'_>) -> Result<Assessment> {
    if !request.enabled || !request.config.squash {
        return Ok(Assessment::Skipped {
            reason: SkipReason::Disabled,
            commit_count: 0,
        });
    }
    let history = HistoryObservation::new(&request.repository.current().path);
    let commit_count = if request.final_history {
        history.count_from_head(request.target)?
    } else {
        0
    };
    if request.final_history && commit_count < 2 && !request.include_staged {
        return Ok(Assessment::Skipped {
            reason: SkipReason::SingleCommit,
            commit_count,
        });
    }
    let generation = &request.config.merge_generation;
    if generation.command.is_none() {
        return Ok(Assessment::Blocked {
            reason: BlockReason::GeneratorMissing,
            commit_count,
        });
    }
    if is_shared(generation) && !trust::is_merge_generation_trusted(request.repository, generation)?
    {
        return Ok(Assessment::Blocked {
            reason: BlockReason::ApprovalRequired,
            commit_count,
        });
    }
    if !request.final_history {
        return Ok(Assessment::PendingFinalHistory);
    }
    Ok(Assessment::Required(RequiredSquash {
        topic_worktree: request.repository.current().path.clone(),
        topic_branch: request.repository.current_branch()?.to_owned(),
        target_branch: request.target.to_owned(),
        commit_count,
        include_staged: request.include_staged,
        generation: generation.clone(),
    }))
}

/// Reobserves final history, generates once, and returns checkpoint-bound state.
pub(crate) fn prepare(required: RequiredSquash) -> Result<Preparation> {
    let repository = RepositoryObservation::new(&required.topic_worktree).repository()?;
    verify_worktree(
        &repository,
        &required.topic_worktree,
        &required.topic_branch,
    )?;
    let history = HistoryObservation::new(&required.topic_worktree);
    let topic_commit = history.head_commit()?;
    let target_commit = history.commit(&required.target_branch)?;
    if !history.is_ancestor(&target_commit, &topic_commit)? {
        bail!(
            "the target changed or is no longer an ancestor of the topic during squash preparation"
        );
    }
    let commit_count = history.count_from_head(&target_commit)?;
    if commit_count < 2 && !required.include_staged {
        return Ok(Preparation::Skipped);
    }
    if required.generation.command.is_none() {
        bail!(missing_generator_message());
    }
    if is_shared(&required.generation)
        && !trust::is_merge_generation_trusted(&repository, &required.generation)?
    {
        bail!(approval_message());
    }
    let expected_result_tree = history.index_tree()?;
    let message = generate_message(
        &repository,
        &required.generation,
        &required.target_branch,
        required.include_staged,
    )?;
    Ok(Preparation::Prepared(PreparedSquash {
        checkpoint: PreparedCheckpoint {
            topic_worktree: required.topic_worktree,
            topic_branch: required.topic_branch,
            target_branch: required.target_branch,
            expected_topic_commit: topic_commit,
            expected_target_commit: target_commit,
            expected_result_tree,
            commit_count,
            include_staged: required.include_staged,
            message,
        },
    }))
}

/// Revalidates prepared facts, owns reset plus commit, and proves the result.
pub(crate) fn collapse(prepared: PreparedSquash) -> Result<CollapsedSquash> {
    let checkpoint = prepared.checkpoint;
    let repository = RepositoryObservation::new(&checkpoint.topic_worktree).repository()?;
    verify_worktree(
        &repository,
        &checkpoint.topic_worktree,
        &checkpoint.topic_branch,
    )?;
    let history = HistoryObservation::new(&checkpoint.topic_worktree);
    if history.head_commit()? != checkpoint.expected_topic_commit
        || history.commit(&checkpoint.target_branch)? != checkpoint.expected_target_commit
        || !history.is_ancestor(
            &checkpoint.expected_target_commit,
            &checkpoint.expected_topic_commit,
        )?
    {
        bail!("squash preparation is stale because topic or target history changed");
    }
    if history.index_tree()? != checkpoint.expected_result_tree {
        bail!("squash preparation is stale because the prepared index tree changed");
    }
    if history.has_staged_changes()? != checkpoint.include_staged {
        bail!("squash preparation is stale because staged-change mode changed");
    }
    let mutation = LifecycleMutation::new(&checkpoint.topic_worktree);
    mutation.reset_soft(&checkpoint.expected_target_commit)?;
    mutation.commit_message(&checkpoint.message)?;
    let commit = history.head_commit()?;
    let facts = history.commit_facts(&commit)?;
    if facts.parents != [checkpoint.expected_target_commit]
        || facts.tree != checkpoint.expected_result_tree
        || facts.message.trim_end() != checkpoint.message
    {
        bail!("squash commit did not match its prepared parent, tree, and message");
    }
    Ok(CollapsedSquash { commit })
}

fn verify_worktree(repository: &Repository, path: &Path, branch: &str) -> Result<()> {
    if repository.current().path != path || repository.current_branch()? != branch {
        bail!("prepared squash no longer matches the registered topic worktree and branch");
    }
    Ok(())
}

fn generate_message(
    repository: &Repository,
    generation: &EffectiveGeneration,
    target: &str,
    include_staged: bool,
) -> Result<String> {
    let command = &generation
        .command
        .as_ref()
        .context(missing_generator_message())?
        .value;
    let template = generation
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
    let diff_source = if include_staged {
        RangeDiffSource::Staged
    } else {
        RangeDiffSource::Committed
    };
    let range = HistoryObservation::new(cwd).range_from_head(target, diff_source)?;
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
            branch, target, repo,
            head_commit => range.head_commit,
            commit_count => range.commit_count,
            commits => range.messages,
            git_diff => range.patch,
            git_diff_stat => range.statistics,
        })
        .context("failed to render the squash generation template")
}

fn is_shared(generation: &EffectiveGeneration) -> bool {
    [generation.command.as_ref(), generation.template.as_ref()]
        .into_iter()
        .flatten()
        .any(|value| value.source == GenerationSource::Shared)
}

const fn missing_generator_message() -> &'static str {
    "no squash message generator is configured; set merge.generation.command or commit.generation.command, or rerun with --no-squash"
}

const fn approval_message() -> &'static str {
    "shared squash message generator approval is required; run pando trust merge-approve, or rerun with --no-squash"
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
