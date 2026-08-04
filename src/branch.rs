//! Authoritative branch classification for worktree navigation and creation.

use anyhow::Result;

use crate::{Worktree, git, git::Repository, worktree_for_branch};

/// The first matching branch category in Pando's resolution order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Classification {
    /// A worktree is already registered for the branch, including exceptional states.
    Registered(Worktree),
    /// A local branch exists without a registered worktree.
    Local,
    /// One or more fetched remote-tracking branches match.
    Remotes(Vec<String>),
    /// No registered, local, or fetched remote branch matches.
    New,
}

/// Classifies `branch` using Pando's established resolution order.
///
/// This function is deliberately deterministic and noninteractive. Callers own
/// intent-specific policy such as `create` refusing a registered worktree or
/// `switch` requiring confirmation before creating a genuinely new branch.
///
/// # Errors
///
/// Returns an error when Git cannot inspect local or remote-tracking refs.
pub(crate) fn classify(repository: &Repository, branch: &str) -> Result<Classification> {
    if let Some(worktree) = worktree_for_branch(&repository.worktrees, branch) {
        return Ok(Classification::Registered(worktree.clone()));
    }

    let cwd = &repository.current().path;
    if git::local_branch_exists(cwd, branch)? {
        return Ok(Classification::Local);
    }

    let remotes = git::remote_matches(cwd, branch)?;
    if remotes.is_empty() {
        Ok(Classification::New)
    } else {
        Ok(Classification::Remotes(remotes))
    }
}
