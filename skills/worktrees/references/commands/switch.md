# `list`, `switch`, `get` — navigation

## `worktrees list`

```
Usage: worktrees list [OPTIONS]
```

No positional args, no subcommands. Prints an aligned table: Git's worktree
order, branch, absolute path, current `*` marker, dirty state, and
exceptional detached, bare, locked, prunable, missing, inaccessible, or
unknown states.

`--output json` is **not supported** — returns a structured "unsupported"
error.

```sh
worktrees list
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

Picker behavior: lists navigable worktrees in Git's order, defaults to the
current worktree, ends with "Create or switch branch…". Escape cancels
without changing directory.

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

`--output json` is **not supported**.

```zsh
worktrees switch feature/login
worktrees switch
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
| `main-worktree-path` | Resolved absolute path of the primary worktree |
| `worktree-root` | Resolved absolute effective configured creation root |

Each successful call writes exactly one value plus a trailing newline.
Branch-dependent properties fail in detached worktrees; `worktree-root`
fails when no root is configured. Works from nested directories.

`--output json` is **not supported**.

```sh
branch=$(worktrees get branch)
path=$(worktrees get worktree-path)
main=$(worktrees get main-worktree-path)
root=$(worktrees get worktree-root)
port=$(worktrees get port)
```

A common use is inside a `post-create` hook (see `../config.md`):

```sh
echo "PORT=$(worktrees get port)" > .env.local
```
