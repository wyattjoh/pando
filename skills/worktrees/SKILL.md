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
| `git worktree add ...` / `git checkout -b ...` | `worktrees switch [branch]` | Applies the branch-resolution order, the configured root, and post-create hooks/trust |
| `git commit -m "<message you wrote>"` | `worktrees commit --input-output json` (omit a message in the request to let the **configured generator** write it) | If the user didn't give you a message, do not invent one yourself — let the generator write it. Only supply a literal message when the user gave you that exact text |
| `git worktree remove ...` | `worktrees remove [--force] [branch...]` | Keeps the local branch ref; runs `pre-remove` hooks |
| `git merge ...` / `git rebase ...` onto the target | `worktrees merge [--no-rebase] [--no-remove]` | Resolves the configured target branch, runs `pre-merge` hooks, and is crash-recoverable |

`git add`/`git add --patch` are still the right tool for **staging** — only
the four operations above must go through `worktrees`, not `git`.

## Local setup

- Binary: `worktrees` on `PATH` (pinned during generation: **v0.1.0**).
- The installed zsh integration wraps `switch`/`remove`/`merge` in a shell
  function that `cd`s to the destination the binary prints; `command
  worktrees ...` bypasses the wrapper and hits the real binary directly.
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

**Only `commit` supports `--output json` / `--input-output json`** today. Every
other command returns a structured "unsupported" error under `--output json`
— drive `list`/`switch`/`get`/`remove`/`merge`/`trust`/`install` normally
and parse their plain stdout (most, like `get`, already print one
unambiguous value).

**When you (an agent) run `worktrees commit`, default to `--input-output
json`**, not the human/Cliclack mode. It gives you a single parsed JSON
document instead of formatted terminal text, a stable `error.code` instead
of a free-text message, and a `next_steps[]` array with ready-to-run
recovery invocations (e.g. what to run if nothing is staged) instead of a
prompt meant for a human. See `references/commands/commit.md` for the full
request/response schema, every `error.code`, and worked examples for staged,
generator-written, `--stage-all`, and dry-run commits. `--input-output json`
requires the **entire** request (`selection`, `message`, `dry_run`) in the
JSON body on stdin — it rejects `-m`/`--stage-all`/`--dry-run` passed as CLI
flags alongside it.

## Anatomy

`worktrees [--output human|json] <command> [command flags]`

## Commands

| Command | Purpose | Reference |
|---|---|---|
| `list` | List worktrees belonging to the current repository | [`references/commands/switch.md`](references/commands/switch.md) |
| `switch [branch]` | Choose, create, or switch to a worktree and print its path | [`references/commands/switch.md`](references/commands/switch.md) |
| `get <property>` | Print one current-worktree property | [`references/commands/switch.md`](references/commands/switch.md) |
| `remove [--force] [branches...]` | Remove one or more topic worktrees while retaining their branches | [`references/commands/lifecycle.md`](references/commands/lifecycle.md) |
| `merge [--no-rebase] [--no-remove]` | Integrate the current topic into the configured target branch | [`references/commands/lifecycle.md`](references/commands/lifecycle.md) |
| `commit [-m MSG] [--stage-all] [--dry-run]` | Commit the existing index, optionally staging every change first | [`references/commands/commit.md`](references/commands/commit.md) |
| `trust <subcommand>` | Inspect or revoke post-create hook or commit-generation approval | [`references/commands/trust.md`](references/commands/trust.md) |
| `install` | Install the managed zsh integration | [`references/commands/install.md`](references/commands/install.md) |

## Common workflows

### Install and enable the zsh integration
```sh
cargo install --path .
worktrees install
source ${ZDOTDIR:-$HOME}/.zshrc
```
Source: README.md ("Install")

### Configure a root before creating any worktrees
No root is ever created automatically — this is required before the first `switch` that creates a worktree.
```yaml
# ${XDG_CONFIG_HOME:-$HOME/.config}/worktrees/config.yaml
worktrees:
  root: ../worktrees
```
Source: README.md ("Global placement")

### Switch to / create a branch worktree
```zsh
worktrees switch feature/login   # exact branch
worktrees switch                 # interactive picker
```
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
main=$(worktrees get main-worktree-path)
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

- [`references/commands/switch.md`](references/commands/switch.md) — `list`, `switch`, `get`
- [`references/commands/lifecycle.md`](references/commands/lifecycle.md) — `remove`, `merge`
- [`references/commands/commit.md`](references/commands/commit.md) — `commit`, including the JSON request/response contract
- [`references/commands/trust.md`](references/commands/trust.md) — `trust` and its subcommands
- [`references/commands/install.md`](references/commands/install.md) — `install`
- [`references/config.md`](references/config.md) — the three-layer YAML config format
