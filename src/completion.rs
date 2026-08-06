//! Candidate producers for dynamic shell completion.
//!
//! Every producer is best-effort and infallible. A completion widget owns the
//! user's command line, so a git failure, a cwd outside a repository, or a
//! malformed ref must yield an empty list rather than an error or any output on
//! stderr.

use std::{
    collections::{HashMap, HashSet},
    env,
    path::PathBuf,
};

use clap_complete::CompletionCandidate;

use crate::{
    WorktreeKind,
    git::{CompletionBranchNames, Repository, RepositoryObservation},
};

/// Branches `switch` accepts: every local branch, plus remote-tracking refs that
/// no local branch already shadows.
#[must_use]
pub fn switch_candidates() -> Vec<CompletionCandidate> {
    let Some(context) = branch_context() else {
        return Vec::new();
    };
    let mut candidates: Vec<_> = context
        .branches
        .local
        .iter()
        .map(CompletionCandidate::new)
        .collect();
    candidates.extend(remote_candidates(
        &context.branches.local,
        &context.branches.remote,
    ));
    candidates
}

/// Branches `create` accepts: those without a registered worktree. `Intent::Create`
/// refuses a branch that already has one.
#[must_use]
pub fn create_candidates() -> Vec<CompletionCandidate> {
    let Some(context) = branch_context() else {
        return Vec::new();
    };
    let registered = registered_branches(&context.repository);
    let mut candidates: Vec<_> = context
        .branches
        .local
        .iter()
        .filter(|branch| !registered.contains(*branch))
        .map(CompletionCandidate::new)
        .collect();
    // Remote refs need no separate exclusion: a registered branch is always a
    // local branch, so `remote_candidates` has already dropped any remote ref
    // shadowed by one.
    candidates.extend(remote_candidates(
        &context.branches.local,
        &context.branches.remote,
    ));
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
    let Ok(repository) = RepositoryObservation::new(&cwd).repository_for_navigation() else {
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

struct BranchContext {
    repository: Repository,
    branches: CompletionBranchNames,
}

fn branch_context() -> Option<BranchContext> {
    let cwd = cwd()?;
    let repository = RepositoryObservation::new(&cwd)
        .repository_for_navigation()
        .ok()?;
    let branches = RepositoryObservation::new(&repository.current().path)
        .completion_branch_names()
        .ok()?;
    Some(BranchContext {
        repository,
        branches,
    })
}

/// Every branch with a registered worktree, primary included.
fn registered_branches(repository: &Repository) -> HashSet<String> {
    repository
        .worktrees
        .iter()
        .filter_map(|worktree| match &worktree.kind {
            WorktreeKind::Branch(branch) => Some(branch.clone()),
            _ => None,
        })
        .collect()
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
fn remote_candidates(local: &[String], remotes: &[String]) -> Vec<CompletionCandidate> {
    let local: HashSet<&str> = local.iter().map(String::as_str).collect();
    let mut grouped: HashMap<&str, Vec<&str>> = HashMap::new();
    for remote in remotes {
        let Some((name, short)) = remote.split_once('/') else {
            continue;
        };
        if !local.contains(short) {
            grouped.entry(short).or_default().push(name);
        }
    }
    let mut candidates: Vec<_> = grouped
        .into_iter()
        .map(|(short, mut remote_names)| {
            remote_names.sort_unstable();
            let help = format!("remote branch ({})", remote_names.join(", "));
            CompletionCandidate::new(short).help(Some(help.into()))
        })
        .collect();
    candidates.sort_by(|a, b| a.get_value().cmp(b.get_value()));
    candidates
}
