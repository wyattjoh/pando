# `trust`

```
Usage: pando trust [OPTIONS] <COMMAND>
```

Inspects or revokes approval for four independently trusted surfaces:
**hook phases** (`post-create`, `pre-merge`, and `pre-remove`), **commit
generation**, **squash-message generation**, and **PR-metadata generation**.
The `*-approve` leaves also grant their generator's approval. No surface has a
noninteractive approval bypass: approval is always an interactive,
default-negative prompt.

| Subcommand | Purpose |
|---|---|
| `status` | Show configured and trusted state for the `post-create`, `pre-merge`, and `pre-remove` hook phases |
| `reset` | Revoke every hook-phase approval for this repository clone |
| `commit-status` | Show approval state for the effective commit-generator settings |
| `commit-reset` | Revoke commit-generator approval for this repository clone |
| `commit-approve` | Preview and approve effective shared commit-generation settings |
| `merge-status` | Show approval state for the effective squash-message-generator settings |
| `merge-reset` | Revoke squash-message-generator approval for this repository clone |
| `merge-approve` | Preview and approve effective shared squash-message-generation settings |
| `pr-status` | Show approval state for the effective PR-metadata-generator settings |
| `pr-reset` | Revoke PR-generator approval for this repository clone |
| `pr-approve` | Approve effective shared PR-generation settings |

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
`${XDG_CONFIG_HOME:-$HOME/.config}/pando/trust.json`. It is never
auto-shared across clones of the same repository.

`reset`/`commit-reset`/`merge-reset` are idempotent and remove only the
current clone's record. After `reset`, the next operation that reaches a
configured hook phase asks again. After `commit-reset`, the next operation
requiring the shared commit generator asks for separate approval. Each surface
is its own namespace: resetting or approving one never alters another, so a
shared `merge.generation` needs `merge-approve` even when the same command is
already approved for `commit`.

`--output json` works for every leaf except `pr-status`/`pr-reset`/`pr-approve`,
which have no structured implementation and answer `trust.json_unsupported`.
Run those three interactively.

## Commands

```sh
pando trust status            # configured/trusted state for every hook phase
pando trust reset             # revoke all hook-phase trust for this clone

pando trust commit-status     # approval state of the effective commit generator
pando trust commit-approve    # interactively approve a shared generator
pando trust commit-reset      # revoke commit-generator trust for this clone

pando trust merge-status      # approval state of the effective squash generator
pando trust merge-approve     # interactively approve a shared squash generator
pando trust merge-reset       # revoke squash-generator trust for this clone
```

## Structured JSON contract

Trust leaves identify as `trust.status`, `trust.reset`, `trust.commit_status`, `trust.commit_reset`, `trust.commit_approve`, `trust.merge_status`, `trust.merge_reset`, and `trust.merge_approve`. Status leaves allow omitted `input`; mutating leaves accept `input.dry_run`. Status reports configured/trusted state, source metadata, counts, and identities without ordinary command contents. Approval previews and approval-required context include the generator settings a person must review. JSON never writes approval: it returns a human-required next step. Reset dry runs emit unattempted effects; real resets distinguish `reset` and `already_reset`. Exact-leaf JSON help is runtime-derived.
