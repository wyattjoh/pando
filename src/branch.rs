//! Authoritative branch classification for worktree navigation and creation.

use std::collections::HashMap;

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

/// Read-only branch facts shared by command planning and completion.
///
/// Facts are one snapshot of registered worktrees, local refs, and already
/// fetched remote-tracking refs. Discovery never fetches or performs any other
/// mutation.
pub(crate) struct Facts<'repository> {
    repository: &'repository Repository,
    local: Vec<String>,
    remotes_by_branch: HashMap<String, Vec<String>>,
}

impl<'repository> Facts<'repository> {
    /// Discovers a complete branch-fact snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when Git cannot inspect local or remote-tracking refs.
    pub(crate) fn discover(repository: &'repository Repository) -> Result<Self> {
        let cwd = &repository.current().path;
        let local = git::discover_branches(cwd)?
            .into_iter()
            .map(|record| record.branch)
            .collect();
        let mut remotes_by_branch: HashMap<String, Vec<String>> = HashMap::new();
        for remote_branch in git::discover_remote_branches(cwd)? {
            let Some((_, branch)) = remote_branch.split_once('/') else {
                continue;
            };
            remotes_by_branch
                .entry(branch.to_owned())
                .or_default()
                .push(remote_branch);
        }
        Ok(Self {
            repository,
            local,
            remotes_by_branch,
        })
    }

    /// Classifies `branch` using Pando's established resolution order.
    #[must_use]
    pub(crate) fn classify(&self, branch: &str) -> Classification {
        if let Some(worktree) = worktree_for_branch(&self.repository.worktrees, branch) {
            return Classification::Registered(worktree.clone());
        }
        if self.local.iter().any(|local| local == branch) {
            return Classification::Local;
        }
        self.remotes_by_branch
            .get(branch)
            .map_or(Classification::New, |remotes| {
                Classification::Remotes(remotes.clone())
            })
    }

    /// Local branch names in Git discovery order.
    pub(crate) fn local(&self) -> &[String] {
        &self.local
    }

    /// Registered worktrees, including the primary worktree.
    pub(crate) fn registered(&self) -> &[Worktree] {
        &self.repository.worktrees
    }

    /// Fetched remote matches grouped by their unqualified branch name.
    pub(crate) fn remotes(&self) -> &HashMap<String, Vec<String>> {
        &self.remotes_by_branch
    }
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
    Ok(Facts::discover(repository)?.classify(branch))
}
