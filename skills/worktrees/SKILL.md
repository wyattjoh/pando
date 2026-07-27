---
name: worktrees
description: 'Kickstart usage of the `worktrees` CLI — inspecting, creating, and navigating Git worktrees. Triggers on "worktrees", "worktrees switch", "worktrees commit", "worktrees get", "worktrees trust", "worktrees merge", "worktrees install", ".worktrees.yaml", ".worktrees.local.yaml", "worktrees config.yaml".'
allowed-tools: Bash, Read
effort: low
---

# worktrees

`worktrees` is a Rust CLI for inspecting, creating, and navigating the Git
worktrees of the repository containing the current directory. Git is the
source of truth — every fact comes from invoking the installed `git`
executable; there is no libgit2, no cached registry, and no implicit fetch.

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
other command returns a structured "unsupported" error under `--output json`.

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

### Stage deliberately, then commit with an explicit message
```sh
git add README.md src/commit.rs   # selected paths
git add --patch                   # selected hunks
worktrees commit -m "feat: add commit support"
```
Bare `worktrees commit` (no `--stage-all`) only commits what is already
staged in the index — it never stages for you.
Source: README.md ("Committing")

### Stage everything and commit, with or without a generated message
```sh
worktrees commit --stage-all -m "chore: commit every change"
worktrees commit --stage-all          # uses the configured generator
```
Source: README.md

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

### Structured (JSON) commit usage — the only JSON-capable command
```sh
worktrees commit --dry-run -m "fix: preview" --output json
printf '%s\n' '{"schema_version":1,"request_id":"job-42","input":{"selection":"staged","message":{"source":"provided","value":"fix: preserve index"},"dry_run":false}}' \
  | worktrees commit --input-output json
worktrees commit --help --output json   # generated request/response schemas
```
Source: README.md ("Structured (JSON) usage"); design rationale in `docs/adr/0002-render-typed-command-outcomes.md`

## References

- [`references/commands/switch.md`](references/commands/switch.md) — `list`, `switch`, `get`
- [`references/commands/lifecycle.md`](references/commands/lifecycle.md) — `remove`, `merge`
- [`references/commands/commit.md`](references/commands/commit.md) — `commit`, including the JSON request/response contract
- [`references/commands/trust.md`](references/commands/trust.md) — `trust` and its subcommands
- [`references/commands/install.md`](references/commands/install.md) — `install`
- [`references/config.md`](references/config.md) — the three-layer YAML config format
