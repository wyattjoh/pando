# `remove`, `clean`, `merge` — topic worktree lifecycle

**Never use raw `git worktree remove` or `git merge`/`git rebase` for these
operations.** `pando remove` keeps the local branch ref and runs
`pre-remove` hooks; `pando clean` is its interactive front end;
`pando merge` resolves the configured target
branch, runs `pre-merge` hooks, and is crash-recoverable across
invocations — a plain `git merge` skips all of that.

## `pando remove [OPTIONS] [BRANCHES...]`

```
Usage: pando remove [OPTIONS] [BRANCHES]...
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

Human runs measure every target in one timed step before mutating, then show a
timed step per removal naming the worktree and how much data is going, and
close with the total reclaimed. Sizes count allocated blocks, count a
hard-linked file once, never follow symlinks, and exclude nested registered
worktrees; `~` marks a total that skipped an unreadable subtree. JSON removal
measures nothing and its contract is unchanged.

Both JSON modes are supported; agents use versioned request mode.

```sh
pando remove                          # (inferred) removes the current topic
pando remove --force feature/login    # (inferred) force-remove a dirty topic
```
No literal example ships in README.md for `remove` — only prose describing
the flags and no-argument behavior; the invocations above combine the
documented flag/positional spellings and are marked **(inferred)**.

## `pando clean [--dry-run]`

```
Usage: pando clean [OPTIONS]
```

| Flag | Purpose |
|---|---|
| `--dry-run` | Open the picker and confirmation as usual, then print the plan instead of mutating |

An interactive multi-select front end over the same removal `pando remove`
performs: same `pre-remove` hooks, same branch retention, same destination on
stdout when the current worktree is among the targets. Human output only —
`--output json` is a hard error, because agents should call `remove` with
explicit branch names instead.

The picker lists every registered worktree except the primary, with a `SIZE`
column giving the disk each one occupies. Sizes are measured in the
background and appear as they land; they count allocated blocks, count a file
reached through a second hard link once, never follow symlinks, and exclude
any nested registered worktree, so a value is the space the removal actually
returns. `~` marks a total that skipped an unreadable subtree.

Each row carries one state glyph — `◼` checked, `◻` unchecked, `✕` not
removable — and the cursor rides the rail as `❯`, so both stay legible with
color disabled.

| Key | Action |
|---|---|
| `↑`/`↓` | Move the cursor |
| `Space` | Toggle the highlighted worktree |
| `Ctrl-A` | Toggle every worktree matching the current filter |
| `Ctrl-S` | Cycle Git order, branch A-Z, last commit newest-first, path A-Z, size largest-first. Size ordering is recomputed only on this key, never as a background measurement lands |
| *(typing)* | Filter by branch, state, or path |
| `Enter` | Remove the checked worktrees; with none checked it exits without removing anything |
| `Esc`/`Ctrl-C` | Cancel |

Locked, prunable, detached, missing, and inaccessible worktrees are listed
with their state but cannot be checked. A worktree with uncommitted changes
*can* be checked; the confirmation then names every such worktree before
discarding its changes, and defaults to "no".

Removal then reports a timed step per worktree naming its size, and the outro
reports the total reclaimed. The picker's measurements are handed to the
removal, so `clean` never walks the same trees twice. Removal is fail-fast, so
a batch that stops partway reports each target as removed, failed, or not
attempted, and suggests rerunning.

```sh
pando clean            # (inferred) pick worktrees to remove
pando clean --dry-run  # (inferred) preview the same selection
```

## `pando merge [OPTIONS]`

```
Usage: pando merge [OPTIONS]
```

| Flag | Purpose |
|---|---|
| `--no-rebase` | Disable the default rebase when the topic has diverged from its target |
| `--no-remove` | Keep the topic worktree/branch after a successful merge |
| `--no-squash` | Merge the topic's commits as they are instead of collapsing them into one |
| `--yolo` | Stage every change and include it in the generated squash commit |

Integrates the current clean topic into the resolved target branch via
`git merge --ff-only`, run in whichever registered worktree has the target
checked out (the primary worktree or a linked one). A diverged
topic rebases onto the target by default. `--yolo` stages every local change
and, when squashing (the default), includes it directly in the generated
squash commit without invoking the commit generator. With `--no-squash`, it
runs the equivalent of `pando commit --stage-all` and continues only if that
configured-generator commit succeeds. It supports human output only and conflicts with `--dry-run`. Phase-specific
`pre-merge` and `pre-remove` hooks run at their lifecycle boundaries (see
`../config.md`).

### Squashing (default)

`merge` collapses the topic into one commit **after** any rebase and
**before** the fast-forward, and generates that commit's message. A topic that
is already a single commit is left untouched, message included. `--no-squash`
opts out for one invocation; `merge.squash: false` opts out by default.

The message generator resolves from `merge.generation.command`, falling back
to `commit.generation.command`. Squashing a multi-commit topic with neither
configured fails rather than merging unsquashed; JSON reports
`merge.squash_generator_missing`. A generator that comes from committed
`.pando.yaml` must be approved first (`pando trust merge-approve`); until then
JSON answers `merge.squash_approval_required`. Both refusals are **preflight**:
they happen before the journal, the rebase, and every other mutation, so a
blocked squash never leaves a rebased topic to recover. See `../config.md`
for the template variables.

The lifecycle journal pins `no_squash` alongside the other policy flags and
records the collapse, so a retry after a later failure never squashes twice.

### In-place merges from the primary worktree

`merge` also runs from the primary worktree when a topic branch is checked
out there directly, with no linked worktree of its own. It rebases and
fast-forwards exactly as usual, then switches the primary worktree to the
target branch and **keeps** the topic branch. Nothing is removed, so
`pre-remove` hooks do not run, `--no-remove` has no additional effect, and no
destination is written to stdout. The plan/context reports `in_place: true`.
Running it from the primary worktree while the target branch itself is checked
out fails with `merge.nothing_to_merge`. If another worktree already has the
target checked out, the primary worktree cannot switch to it and the merge
fails with `merge.target_unavailable` (`problem: checked_out_elsewhere`).

### Target worktree

The fast-forward runs in the one registered worktree that has the target
branch checked out, and a removing merge writes that worktree's path as its
destination. The dry run reports it as `target_worktree` in the context and
names it in human output. Before any mutation, merge refuses with
`merge.target_unavailable` when that worktree cannot be used. The error's
context is typed:

```json
{
  "problem": "not_checked_out",
  "target_branch": "main",
  "target_source": "fallback",
  "primary_worktree": {"encoding": "utf8", "value": "/repo"},
  "primary_branch": "develop",
  "topic_worktree": {"encoding": "utf8", "value": "/repo-feature"},
  "target_worktrees": []
}
```

`problem` is one of `not_checked_out`, `ambiguous`, `checked_out_elsewhere`,
`dirty` (tracked changes; untracked files do not block), `locked`, or
`inaccessible`. `target_source` is `journal`, `local`, `shared`, `global`, or
`fallback`, and the message says "configured", "journaled", or "resolved"
target branch accordingly. `next_steps` offers `worktree.create_target`
(`pando switch <target>`) and `worktree.switch_primary` (`git switch
<target>` in the primary worktree) for a target checked out nowhere, and
`worktree.enter_target` for a dirty, locked, or conflicting one. Merge never
creates, switches, or cleans a target worktree itself.

Uses `worktrees.target-branch` when set in `.pando.yaml` or the global
config. Otherwise, it falls back to the local branch pointed to by
`origin/HEAD`, then local `main`, then local `master`. It errors only when no
configured or fallback branch exists.

```yaml
# .pando.yaml or global config.yaml
worktrees:
  target-branch: main
```

Crash-recoverable and safe to re-invoke: before its first Git mutation,
`merge` records the topic worktree/branch, target branch, and initial
rebase/squash/cleanup policy under Git's common state directory. A per-topic
lease returns `merge.busy` before hooks when another process already owns the
same lifecycle. A later invocation resumes the pinned target/policy, including
continuing through rebase conflicts, recognizing an already-completed
fast-forward, or retrying cleanup without rerunning validated hooks. Source or
target drift returns `merge.stale_plan` rather than replanning.

Human mode prints the generated squash message on the rail before the collapse
runs, using the same bold-subject styling `pando commit` gives its generated
message. Git's own transcript for the squash commit is not echoed; the message
and the fast-forward's diffstat already report it.

Human mode runs the rebase, rebase continuation, message generation, squash,
and fast-forward merge under timed progress indicators and renders Git's own output as terminal UI steps on
stderr rather than streaming it raw. A failure folds the same Git output into
the reported error, so rebase conflicts stay readable. The continuation
neutralizes `GIT_EDITOR`, keeping the commit message Git already recorded.

After the default successful cleanup, stdout contains only the target
worktree's byte-preserving path plus a trailing newline, so the zsh wrapper
can `cd` there. With `--no-remove`, the topic is retained and no destination is
written to stdout. See `docs/adr/0001-journal-merge-lifecycle.md`.

Both JSON modes are supported; agents use versioned request mode.

```sh
pando merge                    # (inferred)
pando merge --no-rebase        # (inferred)
pando merge --no-remove        # (inferred)
pando merge --no-squash        # (inferred) keep the topic's individual commits
pando merge --yolo             # stage every change into one generated squash commit
```
No literal example ships in README.md for `merge` — only prose; flag
spellings are confirmed from the compiled binary's `--help`, but the
invocations above are marked **(inferred)**.

## Structured JSON contract

Agents use request mode. `remove` input contains `branches` and `dry_run`. Force is an argv-only authorization: pass `--force` explicitly after user approval. Never put `force` in the request document. Before any selected target is mutated, `remove` evaluates every target's `pre-remove` hooks through the shared trust policy. An untrusted target returns `trust.approval_required` with `context.approval` (`phase`, ordered named `commands`, repository identity, and command identity), the target branch/path, and an interactive `pando remove <branch>` recovery step. `merge` input contains `no_rebase`, `no_remove`, `no_squash`, and `dry_run`; squashing is on by default in JSON exactly as it is for humans. Request mode rejects mixed command arguments and flags. Exact-leaf removal help includes the runtime `dry_run`/`removed` result schema. Results distinguish dry runs, no-ops, removals, retained topics, in-place merges (`plan: "in_place"`, context `in_place: true`), and completed cleanup. Effects identify hook, Git, destination, and journal actions. Merge cleanup reports `pre_remove_hooks`, `remove_worktree`, `destination`, and `journal_cleanup` independently, while the `squash` effect details carry `applicable`, `commits`, and `trusted`; failures use stable codes with diagnostics and recovery context. Squash-generator stdout and stderr are independently bounded at 64 KiB; overflow fails before validation hooks, fast-forward integration, or removal. **The generated squash message is not part of `result`** — the structured response reports the lifecycle outcome, not the commit's content. It reaches JSON only through the captured `merge` stderr diagnostic. When you need the message itself, read it back from Git with `git log -1 --format=%B <target-branch>`. Dry runs execute no hooks or mutations. Exact-leaf JSON help exposes runtime schemas and the error/action catalogs.
