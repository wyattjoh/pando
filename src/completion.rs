//! Candidate producers for dynamic shell completion.
//!
//! Every producer is best-effort and infallible. A completion widget owns the
//! user's command line, so a git failure, a cwd outside a repository, or a
//! malformed ref must yield an empty list rather than an error or any output on
//! stderr.

use std::{
    collections::HashSet,
    env,
    path::{Path, PathBuf},
};

use clap_complete::CompletionCandidate;

use crate::{WorktreeKind, git};

/// Branches `switch` accepts: every local branch, plus remote-tracking refs that
/// no local branch already shadows.
#[must_use]
pub fn switch_candidates() -> Vec<CompletionCandidate> {
    let Some(cwd) = cwd() else {
        return Vec::new();
    };
    let local = local_branches(&cwd);
    let mut candidates: Vec<_> = local.iter().map(CompletionCandidate::new).collect();
    candidates.extend(remote_candidates(&cwd, &local));
    candidates
}

/// Branches `create` accepts: those without a registered worktree. `Intent::Create`
/// refuses a branch that already has one.
#[must_use]
pub fn create_candidates() -> Vec<CompletionCandidate> {
    let Some(cwd) = cwd() else {
        return Vec::new();
    };
    let registered = registered_branches(&cwd);
    let local = local_branches(&cwd);
    let mut candidates: Vec<_> = local
        .iter()
        .filter(|branch| !registered.contains(*branch))
        .map(CompletionCandidate::new)
        .collect();
    // Remote refs need no separate exclusion: a registered branch is always a
    // local branch, so `remote_candidates` has already dropped any remote ref
    // shadowed by one. Note it is passed the unfiltered local list.
    candidates.extend(remote_candidates(&cwd, &local));
    candidates
}

/// Branches `remove` accepts: those with a registered non-primary worktree. The
/// candidate's help text is the worktree path, matching what `list` shows.
///
/// This mirrors `lifecycle::resolve_targets`, which finds the worktree whose kind
/// is `Branch(name)` and rejects the primary.
#[must_use]
pub fn remove_candidates() -> Vec<CompletionCandidate> {
    let Some(cwd) = cwd() else {
        return Vec::new();
    };
    let Ok(repository) = git::repository(&cwd) else {
        return Vec::new();
    };
    repository
        .worktrees
        .iter()
        .filter(|worktree| Some(&worktree.path) != repository.primary.as_ref())
        .filter_map(|worktree| {
            let WorktreeKind::Branch(branch) = &worktree.kind else {
                return None;
            };
            Some(
                CompletionCandidate::new(branch)
                    .help(Some(worktree.path.display().to_string().into())),
            )
        })
        .collect()
}

fn cwd() -> Option<PathBuf> {
    env::current_dir().ok()
}

fn local_branches(cwd: &Path) -> Vec<String> {
    git::discover_branches(cwd).map_or_else(
        |_| Vec::new(),
        |branches| branches.into_iter().map(|record| record.branch).collect(),
    )
}

/// Every branch with a registered worktree, primary included.
fn registered_branches(cwd: &Path) -> HashSet<String> {
    git::repository(cwd).map_or_else(
        |_| HashSet::new(),
        |repository| {
            repository
                .worktrees
                .iter()
                .filter_map(|worktree| match &worktree.kind {
                    WorktreeKind::Branch(branch) => Some(branch.clone()),
                    _ => None,
                })
                .collect()
        },
    )
}

/// Remote-tracking refs whose short name has no local branch of the same name.
/// `origin/feature` alongside a local `feature` is noise: `switch` resolves the
/// local branch first.
fn remote_candidates(cwd: &Path, local: &[String]) -> Vec<CompletionCandidate> {
    let local: HashSet<&str> = local.iter().map(String::as_str).collect();
    git::discover_remote_branches(cwd).map_or_else(
        |_| Vec::new(),
        |remotes| {
            remotes
                .into_iter()
                .filter(|remote| {
                    remote
                        .split_once('/')
                        .is_some_and(|(_, short)| !local.contains(short))
                })
                .map(|remote| CompletionCandidate::new(remote).help(Some("remote branch".into())))
                .collect()
        },
    )
}
