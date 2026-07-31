//! Candidate producers for dynamic shell completion.
//!
//! Every producer is best-effort and infallible. A completion widget owns the
//! user's command line, so a git failure, a cwd outside a repository, or a
//! malformed ref must yield an empty list rather than an error or any output on
//! stderr.

use std::{
    collections::{HashMap, HashSet},
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
///
/// The candidate *value* is the short branch name (`feature`), not the
/// remote-qualified ref (`origin/feature`). `smart::resolve_and_switch` finds a
/// remote match by probing `refs/remotes/{remote}/{branch}` for the given
/// branch name, so offering `origin/feature` as the value would make `switch`
/// or `create` fail to find that ref and instead create a brand new local
/// branch literally named `origin/feature`, untracked, from the invoking HEAD.
/// The remote is surfaced in the help text instead; ambiguity across multiple
/// remotes offering the same branch is already handled by the interactive
/// `choose_remote` prompt in the resolver, so this only needs to dedupe.
fn remote_candidates(cwd: &Path, local: &[String]) -> Vec<CompletionCandidate> {
    let local: HashSet<&str> = local.iter().map(String::as_str).collect();
    git::discover_remote_branches(cwd).map_or_else(
        |_| Vec::new(),
        |remotes| {
            let mut remotes_by_short: HashMap<String, Vec<String>> = HashMap::new();
            for remote in remotes {
                let Some((remote_name, short)) = remote.split_once('/') else {
                    continue;
                };
                if local.contains(short) {
                    continue;
                }
                remotes_by_short
                    .entry(short.to_string())
                    .or_default()
                    .push(remote_name.to_string());
            }
            let mut candidates: Vec<_> = remotes_by_short
                .into_iter()
                .map(|(short, mut remote_names)| {
                    remote_names.sort();
                    let help = format!("remote branch ({})", remote_names.join(", "));
                    CompletionCandidate::new(short).help(Some(help.into()))
                })
                .collect();
            candidates.sort_by(|a, b| a.get_value().cmp(b.get_value()));
            candidates
        },
    )
}
