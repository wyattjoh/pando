# `list`, `switch`, `create`, `get` — navigation

**Never create or switch worktrees with raw `git worktree add` /
`git checkout -b`.** Use `worktrees switch` or `worktrees create` so branch
resolution, the configured root, and post-create hooks/trust all apply.

## `worktrees list`

```
Usage: worktrees list [OPTIONS]
```

No positional args, no subcommands. Prints aligned `BRANCH`, `LAST COMMIT AT`,
and `PATH` columns with the current `*` marker, dirty state, and exceptional
detached, bare, locked, prunable, missing, inaccessible, or unknown states.
The timestamp is the HEAD commit's committer time, rendered in local
`YYYY-MM-DD HH:MM` form or as `unknown`. Human output starts in the configured
Git, branch A-Z, last-commit newest-first, or path A-Z order and marks the
active order in its heading or column header.

Both JSON modes are supported; agents use versioned request mode.

```sh
worktrees list
```

### `worktrees list -b` / `worktrees list --branches`

Lists local branches (`refs/heads`) instead of worktrees — including a
branch that has never been checked out anywhere. Remote-tracking branches
are never listed and no fetch happens. Each row shows the branch name, the
tip commit's committer timestamp, and the worktree path where the branch is
checked out; the `PATH` cell is blank for a branch with no worktree. A
checked-out branch keeps its current `*` and dirty markers; an unattached
branch shows no condition marker, since there is no working tree to
inspect. Detached and bare worktrees have no branch and so never appear in
this view — it is not a superset of `worktrees list`. Sorting behaves as it
does for worktrees, with unattached branches ordering last under path sort.

```sh
worktrees list --branches
```

## `worktrees switch [BRANCH]`

```
Usage: worktrees switch [OPTIONS] [BRANCH]
```

| Arg | Required | Purpose |
|---|---|---|
| `[BRANCH]` | no | Branch to switch to; omit for the interactive picker |

The binary writes **only** the successful destination path to stdout, with a
trailing newline. Prompts, warnings, and hook output go to stderr — this is
load-bearing: the installed zsh function captures stdout and `cd`s to it.

Picker behavior: lists navigable worktrees with the same branch,
last-commit, and path columns, starts in `worktrees.default-sort`, defaults
to the current worktree, and ends with "Create or switch branch…". Ctrl-S
cycles Git order, branch A-Z, last commit newest-first, and path A-Z for the
current invocation only. Re-sorting preserves the active filter and selected
worktree identity; the create action stays pinned last. Escape cancels
without changing directory.

`worktrees switch -b` / `worktrees switch --branches` opens the picker in
branch view instead of worktree view — the same columns as `worktrees list
--branches`, with the heading and no-matches hint reading "branch" instead
of "worktree". The flag only sets the picker's initial view: `worktrees
switch -b <branch>` behaves exactly like `worktrees switch <branch>` since
no picker opens. Inside the picker, Ctrl-B toggles between worktree view and
branch view for the current invocation only — nothing is persisted, and
there is no configuration key for the default view. Toggling never touches
Git, preserves the typed filter, and keeps the highlighted selection
whenever it exists in both views (a checked-out branch is the same choice
either way); otherwise the selection falls back to the top of the list.
Selecting an already-checked-out branch navigates to its worktree exactly as
worktree view does. Selecting a branch with no worktree creates one for it
through the same resolver `worktrees switch <branch>` uses, including the
post-create hook trust prompt — no new switching or creation path is
introduced.

```zsh
worktrees switch --branches
```

Branch resolution order (no implicit fetch):

1. existing registered worktree for an exact local branch
2. existing local branch → create a worktree for it
3. single already-fetched remote branch of the same name → create a local
   tracking branch
4. multiple matching remotes → prompt which to use
5. genuinely new branch → confirm creation

New branches start from the invoking worktree's committed `HEAD` (including
detached HEAD). Staged/unstaged/untracked changes stay in the source
worktree and only produce a warning — they are never copied. Created
worktrees use the full branch name below the configured root
(`feature/login` → `<root>/feature/login`). Existing destinations and broken
registered worktrees are rejected; the tool never adopts, repairs, prunes,
moves, backs up, or deletes them. Bare repositories can switch among
existing linked worktrees but cannot create new ones.

Both JSON modes are supported; agents use versioned request mode.

```zsh
worktrees switch feature/login
worktrees switch
```

## `worktrees create <BRANCH>`

```
Usage: worktrees create [OPTIONS] <BRANCH>
```

| Arg | Required | Purpose |
|---|---|---|
| `<BRANCH>` | yes | Branch to create a worktree for; there is no picker |

Same resolution, destination, stdout contract, and post-create hooks as
`switch`, with two differences:

- step 5 does **not** confirm. The branch, start point, and destination are
  reported on stderr and creation proceeds, so `create` works without a
  terminal.
- an already-registered branch is a hard error instead of being entered. Use
  `switch` to enter one.

Post-create hook approval is unchanged: untrusted hooks still require an
interactive human, and `create` fails rather than running them unattended.

`--dry-run` previews the destination without creating anything, and refuses
an already-registered branch.

Both JSON modes are supported; agents use versioned request mode. This is
the one machine entry point permitted to create a genuinely new branch
unattended — `switch` still answers `switch.approval_required`.

```zsh
worktrees create feature/login
```

## `worktrees get <PROPERTY>`

```
Usage: worktrees get [OPTIONS] <PROPERTY>
```

`<PROPERTY>` is required, one of:

| Value | Meaning |
|---|---|
| `branch` | Full name of the branch checked out in the containing worktree |
| `port` | Deterministic branch-only port in `10000..=19999` (pinned `SipHasher13`, compatible with Worktrunk v0.66.0 — golden values are asserted in tests; treat as a compatibility contract) |
| `worktree-path` | Resolved absolute path of the containing worktree |
| `primary-worktree-path` | Resolved absolute path of the primary worktree |
| `worktree-root` | Resolved absolute effective configured creation root |

Each successful call writes exactly one value plus a trailing newline.
Branch-dependent properties fail in detached worktrees; `worktree-root`
fails when no root is configured. Works from nested directories.

Both JSON modes are supported; agents use versioned request mode.

```sh
branch=$(worktrees get branch)
path=$(worktrees get worktree-path)
primary=$(worktrees get primary-worktree-path)
root=$(worktrees get worktree-root)
port=$(worktrees get port)
```

A common use is inside a `post-create` hook (see `../config.md`):

```sh
echo "PORT=$(worktrees get port)" > .env.local
```

## Structured JSON contract

Agents use `--input-output json`. `list` accepts `{"schema_version":1,"request_id":"…"}` with omitted `input`, or an empty `input` object. `get` requires `input.property`; JSON names are `branch`, `port`, `worktree_path`, `primary_worktree_path`, and `worktree_root`. `switch` accepts `input.branch`, `input.remote`, and `input.dry_run`. `create` accepts the same input, but `input.branch` is required (`create.branch_required`) and its error codes are namespaced `create.*`. Request mode rejects simultaneous command arguments or flags.

Responses identify as `list`, `get`, `switch`, or `create`; paths are UTF-8/base64 tagged objects. Every structured `list` worktree and `switch.selection_required` choice includes nullable `last_commit_at`, an RFC 3339 HEAD committer timestamp with an explicit offset. These arrays retain Git discovery order under every personal default sort. A systemic metadata failure leaves timestamps null and adds one bounded diagnostic without ordinary stderr. Switch may return an existing destination, a creation plan, or typed selection/remote/approval errors with retry context. Create returns a `created` or `creation_plan` result carrying `kind`, `start_point`, and both `create_branch` and `create_worktree` effects for a genuinely new branch, or fails with `create.branch_registered` plus a `switch` next step. Dry runs perform no creation or hooks and report unattempted effects. Exact-leaf `--help --output json` returns runtime request/response schemas and complete error/action catalogs. JSON writes one document to stdout and no ordinary stderr.

`worktrees list --branches --output json` emits a distinct payload: `result.branches` (not `result.worktrees`), each record carrying `branch`, `head`, nullable `last_commit_at`, nullable `path`, nullable `condition`, and `current`; `path`/`condition` are `null` for a branch with no worktree. `result.summary` reports `total`, `checked_out`, and `dirty`. Branch records are always in `for-each-ref` order — never the caller's personal default sort — regardless of `input-output` mode. `worktrees list --output json` without `--branches` is unchanged.
