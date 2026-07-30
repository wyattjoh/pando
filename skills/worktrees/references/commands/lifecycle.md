# `remove`, `merge` — topic worktree lifecycle

**Never use raw `git worktree remove` or `git merge`/`git rebase` for these
operations.** `worktrees remove` keeps the local branch ref and runs
`pre-remove` hooks; `worktrees merge` resolves the configured target
branch, runs `pre-merge` hooks, and is crash-recoverable across
invocations — a plain `git merge` skips all of that.

## `worktrees remove [OPTIONS] [BRANCHES...]`

```
Usage: worktrees remove [OPTIONS] [BRANCHES]...
```

| Flag/Arg | Purpose |
|---|---|
| `[BRANCHES]...` | Variadic; selects registered topic worktrees to remove. Omit to remove the current topic |
| `--force` | Required to remove a dirty (uncommitted-changes) worktree |

Removes registered topic worktrees but **never** deletes their local branch
refs. When the current worktree is among the removed targets, stdout contains
only the primary worktree's byte-preserving path plus a trailing newline, so
the zsh wrapper can `cd` there. Removing only other worktrees emits no stdout
destination.

Both JSON modes are supported; agents use versioned request mode.

```sh
worktrees remove                          # (inferred) removes the current topic
worktrees remove --force feature/login    # (inferred) force-remove a dirty topic
```
No literal example ships in README.md for `remove` — only prose describing
the flags and no-argument behavior; the invocations above combine the
documented flag/positional spellings and are marked **(inferred)**.

## `worktrees merge [OPTIONS]`

```
Usage: worktrees merge [OPTIONS]
```

| Flag | Purpose |
|---|---|
| `--no-rebase` | Disable the default rebase when the topic has diverged from its target |
| `--no-remove` | Keep the topic worktree/branch after a successful merge |
| `--yolo` | Stage and commit every change with the configured generator before merging |

Integrates the current clean topic into the resolved target branch
(checked out in the primary worktree) via `git merge --ff-only`. A diverged
topic rebases onto the target by default. `--yolo` first runs the equivalent
of `worktrees commit --stage-all` and continues only if the commit succeeds.
It supports human output only and conflicts with `--dry-run`. Phase-specific
`pre-merge` and `pre-remove` hooks run at their lifecycle boundaries (see
`../config.md`).

Uses `worktrees.target-branch` when set in `.worktrees.yaml` or the global
config. Otherwise, it falls back to the local branch pointed to by
`origin/HEAD`, then local `main`, then local `master`. It errors only when no
configured or fallback branch exists.

```yaml
# .worktrees.yaml or global config.yaml
worktrees:
  target-branch: main
```

Crash-recoverable and safe to re-invoke: before its first Git mutation,
`merge` records the topic worktree/branch, target branch, and initial
rebase/cleanup policy under Git's common state directory. A later invocation
resumes the same operation with the pinned target/policy, including continuing
through rebase conflicts or retrying cleanup after a completed integration.

Human mode runs the rebase, rebase continuation, and fast-forward merge under
timed progress indicators and renders Git's own output as terminal UI steps on
stderr rather than streaming it raw. A failure folds the same Git output into
the reported error, so rebase conflicts stay readable. The continuation
neutralizes `GIT_EDITOR`, keeping the commit message Git already recorded.

After the default successful cleanup, stdout contains only the primary
worktree's byte-preserving path plus a trailing newline, so the zsh wrapper
can `cd` there. With `--no-remove`, the topic is retained and no destination is
written to stdout. See `docs/adr/0001-journal-merge-lifecycle.md`.

Both JSON modes are supported; agents use versioned request mode.

```sh
worktrees merge                    # (inferred)
worktrees merge --no-rebase        # (inferred)
worktrees merge --no-remove        # (inferred)
worktrees merge --yolo             # commit every change, then merge
```
No literal example ships in README.md for `merge` — only prose; flag
spellings are confirmed from the compiled binary's `--help`, but the
invocations above are marked **(inferred)**.

## Structured JSON contract

Agents use request mode. `remove` input contains `branches` and `dry_run`. Force is an argv-only authorization: pass `--force` explicitly after user approval. Never put `force` in the request document. `merge` input contains `no_rebase`, `no_remove`, and `dry_run`. Request mode rejects mixed command arguments and flags. Results distinguish dry runs, no-ops, removals, retained topics, and completed cleanup. Effects identify hook, Git, and target worktree actions; failures use stable codes with diagnostics and recovery context. Dry runs execute no hooks or mutations. Exact-leaf JSON help exposes runtime schemas and the error/action catalogs.
