# `trust`

```
Usage: worktrees trust [OPTIONS] <COMMAND>
```

Inspects or revokes approval for two independently-trusted surfaces:
**post-create hooks** and **commit generation**. Neither has a
noninteractive bypass — approval is always an interactive, default-negative
prompt.

| Subcommand | Purpose |
|---|---|
| `status` | Show configured and trusted state for every hook phase |
| `reset` | Revoke every hook-phase approval for this repository clone |
| `commit-status` | Show approval state for the effective commit-generator settings |
| `commit-reset` | Revoke commit-generator approval for this repository clone |
| `commit-approve` | Preview and approve effective shared commit-generation settings |

Subcommand flags/args were not expanded past their one-line descriptions
(depth-2 `--help` limit); each subcommand takes no flags beyond the global
`--output`/`--input-output`/`--help`.

## Trust model

Executable identity is the **ordered `command` strings only** —
`trust::command_hash` digests the ordered `command` fields; step names,
comments, and formatting are excluded on purpose. Reordering or editing a
command revokes approval; renaming a step does not.

Approval is scoped to the **canonical path of this repository clone** and
stored atomically in
`${XDG_CONFIG_HOME:-$HOME/.config}/worktrees/trust.json`. It is never
auto-shared across clones of the same repository.

`reset`/`commit-reset` are idempotent and remove only the current clone's
record — the next worktree creation with commands present asks again.
`commit-reset` does not alter post-create hook approval, and vice versa.

`--output json` is **not supported** for any `trust` subcommand.

## Commands

```sh
worktrees trust status            # what post-create hooks are configured/trusted
worktrees trust reset             # revoke all post-create hook trust for this clone

worktrees trust commit-status     # approval state of the effective commit generator
worktrees trust commit-approve    # interactively approve a shared generator
worktrees trust commit-reset      # revoke commit-generator trust for this clone
```
