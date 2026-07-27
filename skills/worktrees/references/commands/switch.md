# `list`, `switch`, `get` — navigation

**Never create or switch worktrees with raw `git worktree add` /
`git checkout -b`.** Use `worktrees switch` so branch resolution, the
configured root, and post-create hooks/trust all apply.

## `worktrees list`

```
Usage: worktrees list [OPTIONS]
```

No positional args, no subcommands. Prints an aligned table: Git's worktree
order, branch, absolute path, current `*` marker, dirty state, and
exceptional detached, bare, locked, prunable, missing, inaccessible, or
unknown states.

Both JSON modes are supported; agents use versioned request mode.

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

Both JSON modes are supported; agents use versioned request mode.

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

Agents use `--input-output json`. `list` accepts `{"schema_version":1,"request_id":"…"}` with omitted `input`, or an empty `input` object. `get` requires `input.property`; JSON names are `branch`, `port`, `worktree_path`, `primary_worktree_path`, and `worktree_root`. `switch` accepts `input.branch`, `input.remote`, and `input.dry_run`. Request mode rejects simultaneous command arguments or flags.

Responses identify as `list`, `get`, or `switch`; paths are UTF-8/base64 tagged objects. Switch may return an existing destination, a creation plan, or typed selection/remote/approval errors with retry context. Dry runs perform no creation or hooks and report unattempted effects. Exact-leaf `--help --output json` returns runtime request/response schemas and complete error/action catalogs. JSON writes one document to stdout and no ordinary stderr.
