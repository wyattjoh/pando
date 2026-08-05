---
description: Keep Git access behind Pando's concrete capability interfaces
paths:
  - "src/**/*.rs"
  - "tests/**/*.rs"
alwaysApply: false
---

# Git ownership

Use the installed Git executable as the only repository implementation. Construct Git subprocesses only in `src/git.rs`, through its private `GitProcess` execution kernel. The `tests/git_architecture.rs` check rejects direct `Command::new("git")` calls elsewhere in `src/`.

Choose the narrow concrete capability that owns the operation:

- `git::RepositoryObservation` owns repository discovery, current and primary worktree identity, metadata enrichment, ignored-path checks, and repository-level observations.
- `branch::Snapshot` owns immutable branch/ref classification, target and base-ref planning, fetch applicability, upstream and remote selection, and publication planning. `git::RefMutation` narrowly owns explicit ref fetch and push mutation.
- `git::WorktreeMutation` owns worktree creation, branch descriptions, destination safety, and removal while retaining branches.
- `git::HistoryObservation` owns commit identities and messages, commit counts, staged and range diffs, statistics, ancestry, and recent subjects.
- `git::LifecycleMutation` owns switching for lifecycle work, rebase and continuation, fast-forward merge, soft reset, staging, commit creation, and rebase-state observation.
- Private parsers in `git.rs` own structured Git output, especially byte-safe NUL-delimited worktree porcelain. Test parser edge cases in the module without introducing a repository fake.
- Private `GitProcess` owns executable selection, working directory, environment, stdin, stream routing, exit handling, and process errors.

Expose semantic operations and typed results. For example, add a capability method shaped like:

```rust
pub(crate) fn rebase_in_progress(self) -> Result<bool>;
```

Do not expose command-shaped forwarding APIs or raw process policy:

```rust
// Forbidden: callers assemble Git and interpret subprocess details.
pub(crate) fn run_git(args: &[&str], inherit_output: bool) -> Result<Output>;
```

Keep new interfaces crate-internal unless an existing external library contract requires public access. Do not add a public Git trait, fake adapter, cache, registry, libgit2 adapter, or alternate repository implementation. Behavioral tests must use real temporary Git repositories as the source of truth.
