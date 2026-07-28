---
name: worktrees
description: 'Kickstart usage of the `worktrees` CLI — inspecting, creating, and navigating Git worktrees. Triggers on "worktrees", "worktrees switch", "worktrees commit", "worktrees get", "worktrees trust", "worktrees merge", "worktrees install", ".worktrees.yaml", ".worktrees.local.yaml", "worktrees config.yaml".'
allowed-tools: Bash, Read
effort: medium
---

# worktrees

`worktrees` is a Rust CLI for inspecting, creating, and navigating the Git
worktrees of the repository containing the current directory. Git is the
source of truth — every fact comes from invoking the installed `git`
executable; there is no libgit2, no cached registry, and no implicit fetch.

## Use the CLI, not raw git — this is the point of the skill

In a repository that uses `worktrees`, four operations are wrapped by a
`worktrees` subcommand instead of being done with plain `git`. Use the
subcommand, never the raw git equivalent, even though the raw command would
often "work":

| Don't | Do | Why the wrapper matters |
|---|---|---|
| `git worktree add ...` / `git checkout -b ...` | `worktrees switch [--dry-run] [branch]` | Applies the branch-resolution order, the configured root, and post-create hooks/trust |
| `git commit -m "<message you wrote>"` | `worktrees commit --input-output json` (omit a message in the request to let the **configured generator** write it) | If the user didn't give you a message, do not invent one yourself — let the generator write it. Only supply a literal message when the user gave you that exact text |
| `git worktree remove ...` | `worktrees remove [--force] [--dry-run] [branch...]` | Keeps the local branch ref; runs `pre-remove` hooks |
| `git merge ...` / `git rebase ...` onto the target | `worktrees merge [--no-rebase] [--no-remove] [--yolo] [--dry-run]` | Resolves the configured target branch, runs `pre-merge` hooks, and is crash-recoverable |

`git add`/`git add --patch` are still the right tool for **staging** — only
the four operations above must go through `worktrees`, not `git`.

## Local setup

- Binaries: `worktrees` and its `wt` symlink on `PATH` when installed with `just install` (pinned during generation: **v0.1.0**).
- The installed zsh integration wraps both `worktrees` and `wt` so
  `switch`/`remove`/`merge` can `cd` to the destination the selected binary
  prints. `command worktrees ...` and `command wt ...` bypass the wrappers.
- Config files the CLI reads (see `references/config.md`):
  `${XDG_CONFIG_HOME:-$HOME/.config}/worktrees/config.yaml`,
  `.worktrees.yaml`, `.worktrees.local.yaml`.

## Global flags

| Flag | Values | Purpose |
|---|---|---|
| `--output <OUTPUT>` | `human` (default), `json` | Human terminal output or one structured JSON document |
| `--input-output <INPUT_OUTPUT>` | `json` only | Read a versioned JSON request from stdin, emit JSON. `human` is a hard error |
| `-h`, `--help` | — | Print help (combine with `--output json` for generated JSON Schemas) |
| `-V`, `--version` | — | Print version |

| Env var | Purpose |
|---|---|
| `XDG_CONFIG_HOME` | Base for `worktrees/config.yaml`, `trust.json`, generated zsh integration. Falls back to `$HOME/.config` |
| `ZDOTDIR` | Where `worktrees install` writes/edits `.zshrc`. Falls back to `$HOME` |

**Agents must use `--input-output json` for every normal operation.** Put the
leaf command (and trust subcommand) in argv and the complete command input in
the strict version-1 stdin envelope. Do not parse human tables, messages, or
scalar stdout. Read `status`, typed `result`/`error`, `effects`, bounded
`diagnostics`, and executable `next_steps` instead. Use human commands only
when a returned next step explicitly requires a person's approval.

Requests reject unknown fields, trailing data, unsupported versions, and
mixed command flags. Dry-run mutating requests use `input.dry_run:true`.
Force is argv-only authorization for removal. Pass `--force` only with explicit user approval; never put `force` in a remove request document. JSON cannot grant hook or generator trust or authorize installer
writes. Structured worktree records expose nullable RFC 3339
`last_commit_at` values and always retain Git discovery order, regardless of
the human `worktrees.default-sort` preference. See the command references
for leaf contracts and approval rules.

## Anatomy

`worktrees [--output human|json] <command> [command flags]`

## Commands

| Command | Purpose | Reference |
|---|---|---|
| `list` | List worktrees belonging to the current repository | [`references/commands/switch.md` (navigation)](references/commands/switch.md) |
| `switch [--dry-run] [branch]` | Choose, create, or switch to a worktree and print its path | [`references/commands/switch.md` (navigation)](references/commands/switch.md) |
| `get <property>` | Print one current-worktree property | [`references/commands/switch.md` (navigation)](references/commands/switch.md) |
| `remove [--force] [--dry-run] [branches...]` | Remove one or more topic worktrees while retaining their branches | [`references/commands/lifecycle.md`](references/commands/lifecycle.md) |
| `merge [--no-rebase] [--no-remove] [--yolo] [--dry-run]` | Integrate the current topic into the configured target branch | [`references/commands/lifecycle.md`](references/commands/lifecycle.md) |
| `commit [-m MSG] [--stage-all] [--dry-run]` | Commit the existing index, optionally staging every change first | [`references/commands/commit.md`](references/commands/commit.md) |
| `trust [--dry-run] <subcommand>` | Inspect, approve, or revoke hook-phase or commit-generation trust. Subcommands: `status`, `reset`, `commit-status`, `commit-reset`, `commit-approve` | [`references/commands/trust.md`](references/commands/trust.md) |
| `install [--dry-run]` | Install or preview the managed zsh integration | [`references/commands/install.md`](references/commands/install.md) |

## Common workflows

### Install and enable the zsh integration
```sh
just install
worktrees install
source ${ZDOTDIR:-$HOME}/.zshrc
```
`just install` installs `worktrees` with Cargo and creates `wt` as a relative symlink beside it.
Source: README.md ("Install")

### Configure a root before creating any worktrees
No root is ever created automatically — this is required before the first `switch` that creates a worktree.
```yaml
# ${XDG_CONFIG_HOME:-$HOME/.config}/worktrees/config.yaml
worktrees:
  root: ../worktrees
  default-sort: last-commit-at # git, branch, last-commit-at, or path
```
The ignored `.worktrees.local.yaml` overlay may override both values; committed
`.worktrees.yaml` cannot set the personal `default-sort` preference.
Source: README.md ("Global placement")

### Switch to / create a branch worktree
```zsh
worktrees switch feature/login   # exact branch
worktrees switch                 # interactive picker
```
The picker shows local HEAD committer timestamps and starts in the configured
sort mode. Ctrl-S cycles Git order, branch A-Z, last commit newest-first, and
path A-Z without persisting the change or losing the filter/selection.
Source: README.md ("Switching and creating")

### Stage deliberately, then commit as an agent — `--input-output json`
```sh
git add README.md src/commit.rs   # selected paths, or:
git add --patch                   # selected hunks

# no message given: let the configured generator write it
printf '%s\n' '{"schema_version":1,"input":{"selection":"staged","message":{"source":"configured_generator"},"dry_run":false}}' \
  | worktrees commit --input-output json

# user gave you this exact message
printf '%s\n' '{"schema_version":1,"input":{"selection":"staged","message":{"source":"provided","value":"feat: add commit support"},"dry_run":false}}' \
  | worktrees commit --input-output json
```
Bare `worktrees commit` (`"selection":"staged"`) only commits what is
already staged in the index — it never stages for you. If the user asked
you to "commit" without giving an exact message, **do not compose one
yourself** — send `{"source":"configured_generator"}` so the configured
generator (or the CLI's own built-in prompt, if none is configured) writes
it. Only send `{"source":"provided","value":"..."}` when the user gave you
that literal text.
Source: README.md ("Committing"); request schema confirmed against
`src/commit.rs`'s `CommitRequest`/`MessageSource` types

### Stage everything and commit, with or without a generated message
```sh
# generator writes the message
printf '%s\n' '{"schema_version":1,"input":{"selection":"stage_all","message":{"source":"configured_generator"},"dry_run":false}}' \
  | worktrees commit --input-output json

# user gave you this exact message
printf '%s\n' '{"schema_version":1,"input":{"selection":"stage_all","message":{"source":"provided","value":"chore: commit every change"},"dry_run":false}}' \
  | worktrees commit --input-output json
```
Source: README.md; `Selection::StageAll` in `src/commit.rs`

### Preview a commit before running it (dry run)
```sh
printf '%s\n' '{"schema_version":1,"input":{"selection":"staged","message":{"source":"configured_generator"},"dry_run":true}}' \
  | worktrees commit --input-output json
```
On success, `result` is `{"outcome":"dry_run","ready":true,"selection":"staged"}` —
nothing is staged, generated, or committed.
Source: `src/commit.rs` (`run_json`'s `invocation.dry_run` branch)

### Add a shared post-create hook
```yaml
# .worktrees.yaml (committed, read from the invoking worktree)
hooks:
  post-create:
    - name: Install dependencies
      command: npm install
    - command: echo "PORT=$(worktrees get port)" > .env.local
```
New shared hook commands are untrusted until approved:
```sh
worktrees trust status
worktrees trust reset
```
Source: README.md ("Shared project setup", "Trust")

### Query current-worktree properties (e.g. from a hook script)
```sh
branch=$(worktrees get branch)
path=$(worktrees get worktree-path)
primary=$(worktrees get primary-worktree-path)
root=$(worktrees get worktree-root)
port=$(worktrees get port)
```
Source: README.md ("get")

### Handle a `commit` JSON error response
```sh
printf '%s\n' '{"schema_version":1,"input":{"selection":"staged","message":{"source":"configured_generator"},"dry_run":false}}' \
  | worktrees commit --input-output json
# {"status":"error","error":{"code":"commit.nothing_staged","message":"nothing is staged"},"next_steps":[...]}
```
On `"status":"error"`, read `error.code` and, when present, `next_steps[]` —
each entry is a ready-to-run `{"action","description","invocation":{"argv","stdin"}}`
recovery option (e.g. `commit.nothing_staged` suggests staging paths, staging
a patch, or retrying with `"selection":"stage_all"`). Don't guess a recovery
step; use the one the response gives you. Full error-code table in
`references/commands/commit.md`.
Source: `src/commit.rs` (`recovery_steps`, `emit_failure_with_context`); design
rationale in `docs/adr/0002-render-typed-command-outcomes.md`

### Inspect the JSON request/response schema and Schema version
```sh
worktrees commit --help --output json   # generated request/response JSON Schemas
```
Source: README.md ("Structured (JSON) usage")

## References

- [`references/commands/switch.md`](references/commands/switch.md) — navigation commands: `list`, `switch`, `get`
- [`references/commands/lifecycle.md`](references/commands/lifecycle.md) — `remove`, `merge`
- [`references/commands/commit.md`](references/commands/commit.md) — `commit`, including the JSON request/response contract
- [`references/commands/trust.md`](references/commands/trust.md) — `trust` and its subcommands
- [`references/commands/install.md`](references/commands/install.md) — `install`
- [`references/config.md`](references/config.md) — the three-layer YAML config format
