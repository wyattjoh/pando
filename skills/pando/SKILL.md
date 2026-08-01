---
name: pando
description: 'Kickstart usage of the `pando` CLI — inspecting, creating, and navigating Git worktrees. Triggers on "pando", "worktrees", "pando switch", "pando create", "pando commit", "pando get", "pando trust", "pando merge", "pando install", ".pando.yaml", ".pando.local.yaml", "pando config.yaml".'
allowed-tools: Bash, Read
effort: medium
---

# pando

`pando` is a Rust CLI for inspecting, creating, and navigating the Git
worktrees of the repository containing the current directory. Git is the
source of truth — every fact comes from invoking the installed `git`
executable; there is no libgit2, no cached registry, and no implicit fetch.

## Use the CLI, not raw git — this is the point of the skill

In a repository that uses `pando`, four operations are wrapped by a
`pando` subcommand instead of being done with plain `git`. Use the
subcommand, never the raw git equivalent, even though the raw command would
often "work":

| Don't | Do | Why the wrapper matters |
|---|---|---|
| `git worktree add ...` / `git checkout -b ...` | `pando switch [--dry-run] [branch]`, or `pando create [--dry-run] <branch>` to skip the new-branch confirmation | Applies the branch-resolution order, the configured root, and post-create hooks/trust |
| `git commit -m "<message you wrote>"` | `pando commit --input-output json` (omit a message in the request to let the **configured generator** write it) | If the user didn't give you a message, do not invent one yourself — let the generator write it. Only supply a literal message when the user gave you that exact text |
| `git worktree remove ...` | `pando remove [--force] [--dry-run] [branch...]` | Keeps the local branch ref; runs `pre-remove` hooks |
| `git merge ...` / `git rebase ...` onto the target | `pando merge [--no-rebase] [--no-remove] [--yolo] [--dry-run]` | Resolves the configured target branch, or origin/HEAD, main, then master without fetching, runs `pre-merge` hooks, and is crash-recoverable |

`git add`/`git add --patch` are still the right tool for **staging** — only
the four operations above must go through `pando`, not `git`.

## Local setup

- Binaries: `pando` and its `pd` symlink on `PATH` when installed with Homebrew or `just install` (pinned during generation: **v0.1.1**).
- The installed zsh integration wraps both `pando` and `pd` so
  `switch`/`create`/`remove`/`merge` can `cd` to the destination the selected binary
  prints. `command pando ...` and `command pd ...` bypass the wrappers.
- Config files the CLI reads (see `references/config.md`):
  `${XDG_CONFIG_HOME:-$HOME/.config}/pando/config.yaml`,
  `.pando.yaml`, `.pando.local.yaml`.

## Global flags

| Flag | Values | Purpose |
|---|---|---|
| `--output <OUTPUT>` | `human` (default), `json` | Human terminal output or one structured JSON document |
| `--input-output <INPUT_OUTPUT>` | `json` only | Read a versioned JSON request from stdin, emit JSON. `human` is a hard error |
| `-h`, `--help` | — | Print help (combine with `--output json` for generated JSON Schemas) |
| `-V`, `--version` | — | Print version |

| Env var | Purpose |
|---|---|
| `XDG_CONFIG_HOME` | Base for `pando/config.yaml`, `trust.json`, generated zsh integration. Falls back to `$HOME/.config` |
| `ZDOTDIR` | Where `pando install` writes/edits `.zshrc`. Falls back to `$HOME` |

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
the human `worktrees.default-sort` preference. `pando list --branches
--output json` emits a distinct `result.branches` payload (branch, head,
nullable `last_commit_at`/`path`/`condition`, `current`) in `for-each-ref`
order, also independent of the personal default sort; `path`/`condition`
are `null` for a branch with no worktree. See the command references
for leaf contracts and approval rules.

## Anatomy

`pando [--output human|json] <command> [command flags]`

## Commands

| Command | Purpose | Reference |
|---|---|---|
| `list [-b\|--branches]` | List worktrees belonging to the current repository, or local branches (including unattached ones) with `--branches` | [`references/commands/switch.md` (navigation)](references/commands/switch.md) |
| `switch [-b\|--branches] [--fetch] [--dry-run] [branch]` | Choose, create, or switch to a worktree and print its path; `--branches` opens the picker in branch view, `--fetch` refreshes the fresh base ref | [`references/commands/switch.md` (navigation)](references/commands/switch.md) |
| `create [--fetch] [--dry-run] <branch>` | Create a worktree and print its path, without confirming a new branch | [`references/commands/switch.md` (navigation)](references/commands/switch.md) |
| `get <property>` | Print one current-worktree property | [`references/commands/switch.md` (navigation)](references/commands/switch.md) |
| `remove [--force] [--dry-run] [branches...]` | Remove one or more topic worktrees while retaining their branches | [`references/commands/lifecycle.md`](references/commands/lifecycle.md) |
| `merge [--no-rebase] [--no-remove] [--yolo] [--dry-run]` | Integrate the current topic into the configured target branch | [`references/commands/lifecycle.md`](references/commands/lifecycle.md) |
| `commit [-m MSG] [--stage-all] [--dry-run]` | Commit the existing index, optionally staging every change first | [`references/commands/commit.md`](references/commands/commit.md) |
| `trust [--dry-run] <subcommand>` | Inspect, approve, or revoke hook-phase or commit-generation trust. Subcommands: `status`, `reset`, `commit-status`, `commit-reset`, `commit-approve` | [`references/commands/trust.md`](references/commands/trust.md) |
| `install [--dry-run]` | Install or preview the managed zsh integration (including dynamic tab completion) and global config scaffold | [`references/commands/install.md`](references/commands/install.md) |
| `pr create` | Create a draft or ready pull request from a published topic branch | [`references/commands/pr.md`](references/commands/pr.md) |

## Common workflows

### Install and enable the zsh integration
```sh
brew install wyattjoh/stable/pando
pando install
source ${ZDOTDIR:-$HOME}/.zshrc
```
To build from source, run `just install` before `pando install` instead. Homebrew and `just install` both provide `pando` plus its `pd` symlink. `pando install` adds an idempotent commented scaffold to the global config without enabling or overwriting user settings.
Source: README.md ("Install")

### Configure a root before creating any worktrees
No root is ever created automatically — this is required before the first `switch` that creates a worktree.
```yaml
# ${XDG_CONFIG_HOME:-$HOME/.config}/pando/config.yaml
worktrees:
  root: ../worktrees
  default-sort: last-commit-at # git, branch, last-commit-at, or path
  base: head                   # head (default) or fresh
```
The ignored `.pando.local.yaml` overlay may override all three values;
committed `.pando.yaml` cannot set the personal `default-sort` preference,
though it may set `base` and `target-branch`.
Source: README.md ("Global placement")

### Switch to / create a branch worktree
```zsh
pando switch feature/login   # exact branch
pando switch                 # interactive picker
pando switch --branches      # interactive picker, starting in branch view
pando create feature/login   # create without confirming; fails if it exists
pando create --fetch topic   # refresh the fresh base ref first (base: fresh only)
```
New branches start at the invoking worktree's `HEAD` unless `worktrees.base`
is `fresh`, which cuts them from the target branch's remote-tracking ref
instead. Nothing is fetched implicitly; `--fetch` refreshes exactly that one
ref. See [`references/config.md`](references/config.md).
The picker shows local HEAD committer timestamps and starts in the configured
sort mode. Ctrl-S cycles Git order, branch A-Z, last commit newest-first, and
path A-Z without persisting the change or losing the filter/selection. Ctrl-B
toggles between worktree view and branch view (local branches, including
ones with no worktree yet) for the current invocation only — selecting an
unattached branch creates a worktree for it through the same resolver
`pando switch <branch>` uses.
Source: README.md ("Switching and creating")

### Stage deliberately, then commit as an agent — `--input-output json`
```sh
git add README.md src/commit.rs   # selected paths, or:
git add --patch                   # selected hunks

# no message given: let the configured generator write it
printf '%s\n' '{"schema_version":1,"input":{"selection":"staged","message":{"source":"configured_generator"},"dry_run":false}}' \
  | pando commit --input-output json

# user gave you this exact message
printf '%s\n' '{"schema_version":1,"input":{"selection":"staged","message":{"source":"provided","value":"feat: add commit support"},"dry_run":false}}' \
  | pando commit --input-output json
```
Bare `pando commit` (`"selection":"staged"`) only commits what is
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
  | pando commit --input-output json

# user gave you this exact message
printf '%s\n' '{"schema_version":1,"input":{"selection":"stage_all","message":{"source":"provided","value":"chore: commit every change"},"dry_run":false}}' \
  | pando commit --input-output json
```
Source: README.md; `Selection::StageAll` in `src/commit.rs`

### Preview a commit before running it (dry run)
```sh
printf '%s\n' '{"schema_version":1,"input":{"selection":"staged","message":{"source":"configured_generator"},"dry_run":true}}' \
  | pando commit --input-output json
```
On success, `result` is `{"outcome":"dry_run","ready":true,"selection":"staged"}` —
nothing is staged, generated, or committed.
Source: `src/commit.rs` (`run_json`'s `invocation.dry_run` branch)

### Add a shared post-create hook
```yaml
# .pando.yaml (committed, read from the invoking worktree)
hooks:
  post-create:
    - name: Install dependencies
      command: npm install
    - command: echo "PORT=$(pando get port)" > .env.local
```
New shared hook commands are untrusted until approved:
```sh
pando trust status
pando trust reset
```
Source: README.md ("Shared project setup", "Trust")

### Query current-worktree properties (e.g. from a hook script)
```sh
branch=$(pando get branch)
path=$(pando get worktree-path)
primary=$(pando get primary-worktree-path)
root=$(pando get worktree-root)
port=$(pando get port)
```
Source: README.md ("get")

### Handle a `commit` JSON error response
```sh
printf '%s\n' '{"schema_version":1,"input":{"selection":"staged","message":{"source":"configured_generator"},"dry_run":false}}' \
  | pando commit --input-output json
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

### Create a pull request from a fork

Use `pando pr create --remote <name>` (or `input.remote` in JSON) when the
head branch belongs on a personal fork. Remote precedence is explicit remote,
then the branch upstream, then `origin`, then a sole configured remote. The
the resolved target branch must have an upstream GitHub remote, which is used as
the base repository. Fork heads are sent to GitHub as `owner:branch`; preflight
and dry-run resolve both repositories before any push.

### Inspect the JSON request/response schema and Schema version
```sh
pando commit --help --output json   # generated request/response JSON Schemas
```
Source: README.md ("Structured (JSON) usage")

## References

- [`references/commands/switch.md`](references/commands/switch.md) — navigation commands: `list`, `switch`, `create`, `get`
- [`references/commands/lifecycle.md`](references/commands/lifecycle.md) — `remove`, `merge`
- [`references/commands/commit.md`](references/commands/commit.md) — `commit`, including the JSON request/response contract
- [`references/commands/trust.md`](references/commands/trust.md) — `trust` and its subcommands
- [`references/commands/install.md`](references/commands/install.md) — `install`
- [`references/config.md`](references/config.md) — the three-layer YAML config format
