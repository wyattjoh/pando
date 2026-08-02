# `commit`

**Never run `git commit` directly in a repo that uses `pando`.** The
commit itself — staged-only or `--stage-all` — must go through `pando
commit`. If the caller didn't give you an exact commit message, don't
compose one yourself: let the configured generator (or the CLI's built-in
prompt, if none is configured) write it.

**As an agent, prefer `--input-output json` over the human/Cliclack mode**
for every `commit` invocation. It replaces formatted terminal text with one
parseable JSON document, a stable `error.code` instead of a free-text
message, and (on the recoverable error above all) a `next_steps[]` array of
ready-to-run recovery invocations — you don't have to guess what to try
next. All executable leaves use the shared structured protocol.

```
Usage: pando commit [OPTIONS]
```

| Flag | Purpose |
|---|---|
| `-m`, `--message <MESSAGE>` | Commit message. Omit to use the configured generator. Supplying it bypasses all generator configuration, template validation, and generator trust |
| `--stage-all` | Stage tracked, deleted, and untracked changes before committing |
| `--dry-run` | Validate and preview without staging, generating, running hooks, changing trust, or committing |
| `--output <human\|json>` | One-shot: normal CLI flags in, one JSON document out |
| `--input-output json` | Full request/response: the **entire** request (`selection`, `message`, `dry_run`) comes from a JSON document on stdin — `-m`/`--stage-all`/`--dry-run` are rejected alongside it (`json.invalid_request`: "command options are forbidden with --input-output json") |

## Behavior

- **`"selection":"staged"`** (bare `pando commit` in human mode) commits
  only the existing Git index. Stage deliberately first with
  `git add <paths>` or `git add --patch`.
- **`"selection":"stage_all"`** (`--stage-all` in human mode) stages every
  tracked/deleted/untracked change before committing.
- In human mode, an interactive bare commit that finds a dirty worktree but
  an **empty** index previews the all-change candidate and offers a
  default-No confirmation before staging everything. JSON mode never prompts
  — it fails with `commit.nothing_staged` instead (see below).
- **`{"source":"configured_generator"}`** (no `-m` in human mode) renders the
  staged snapshot into a MiniJinja prompt sent on stdin to the configured
  generator command (`/bin/sh -c`, run from the worktree root, ordinary
  environment/`PATH`). The generator's stdout becomes the commit message.
  Git's normal hooks, signing, and failure behavior remain enabled. A
  generator failure after staging everything leaves the all-changes snapshot
  staged for inspection or retry.
- Shared (`.pando.yaml`) generator fields (`command`/`template`) are
  **untrusted** and must be approved via `pando trust commit-approve`
  (interactive, default-negative) before they can win — **JSON requests
  cannot approve a generator**; that step must happen interactively first,
  or the JSON request fails with `trust.approval_required`. User global/local
  generator values need no approval.

## Agent usage — `--input-output json`

Request envelope (`CommitRequestEnvelope` in `src/commit.rs`):

```jsonc
{
  "schema_version": 1,               // required; only 1 is supported today
  "request_id": "job-42",            // optional, echoed back verbatim
  "input": {
    "selection": "staged",           // "staged" | "stage_all"
    "message": { "source": "configured_generator" },
    // or: { "source": "provided", "value": "feat: exact message" }
    "dry_run": false
  }
}
```

```sh
# Staged only, generator writes the message
printf '%s\n' '{"schema_version":1,"input":{"selection":"staged","message":{"source":"configured_generator"},"dry_run":false}}' \
  | pando commit --input-output json

# Staged only, exact message you were given
printf '%s\n' '{"schema_version":1,"input":{"selection":"staged","message":{"source":"provided","value":"feat: add commit support"},"dry_run":false}}' \
  | pando commit --input-output json

# Stage everything, generator writes the message
printf '%s\n' '{"schema_version":1,"input":{"selection":"stage_all","message":{"source":"configured_generator"},"dry_run":false}}' \
  | pando commit --input-output json

# Dry run: validate and preview, no mutation
printf '%s\n' '{"schema_version":1,"input":{"selection":"staged","message":{"source":"configured_generator"},"dry_run":true}}' \
  | pando commit --input-output json
```

### Response envelope

```jsonc
{
  "schema_version": 1,
  "request_id": "job-42",            // echoes your request_id, if any
  "command": "commit",
  "status": "success",               // "success" | "error"
  "result": { "outcome": "committed", "commit": "<hash>", "selection": "staged" },
  // dry run success instead: {"outcome":"dry_run","ready":true,"selection":"staged"}
  "error": null,                     // {"code": "...", "message": "..."} on failure
  "context": { "repository": {...}, "changes": {...} },  // current staged/unstaged/untracked, per git status
  "effects": [ { "action": "commit.create", "attempted": true, "completed": true } ],
  "diagnostics": [],                 // captured generator/git stdout+stderr; appears on success too (e.g. git commit's own stdout)
  "next_steps": []                   // populated on some errors; see below
}
```

On `"status":"error"`, read `error.code` first, then `next_steps[]` if
present — each entry is `{"action","description","mutation","requires_human_approval","invocation":{"argv","stdin"}}`,
a ready-to-run recovery command. Don't hand-construct a recovery request;
use the one the response gives you.

### Error codes

| `error.code` | Meaning | Typical recovery |
|---|---|---|
| `json.invalid_request` | Malformed JSON, or CLI flags combined with `--input-output json` | Fix the request body |
| `json.unsupported_schema_version` | `schema_version` isn't `1` | Use schema version 1 |
| `repository.invalid` | Not inside a Git repository | n/a |
| `repository.bare` | Current repository is bare | `commit` requires a worktree |
| `commit.nothing_to_commit` | Working tree is fully clean (nothing staged, nothing dirty) — for either selection; also returned for `stage_all` whenever nothing is dirty | n/a |
| `commit.nothing_staged` | `staged` requested, index is empty, but the worktree **is** dirty elsewhere | `next_steps` offers `git.stage_paths`, `git.stage_patch` (human approval required), or retrying with `"selection":"stage_all"` |
| `trust.approval_required` | Shared generator config isn't approved yet | Run `pando trust commit-approve` interactively, then retry |
| `commit.generator_unavailable` | No generator configured but none was `provided` either | Supply `{"source":"provided","value":"..."}` or configure a generator |
| `commit.preflight_failed` | Generator/template config invalid | Fix `.pando.yaml`/config generator fields |
| `commit.staging_failed` | `git add -A`-equivalent staging failed | Inspect `diagnostics` |
| `commit.generator_failed` | Generator command exited nonzero or couldn't spawn | Inspect `diagnostics`, fix or bypass the generator |
| `commit.generator_invalid_output` | Generator produced empty or non-UTF-8 output | Fix the generator, or supply `{"source":"provided",...}` |
| `commit.invalid_message` | Provided message was empty after trimming | Provide a non-empty message |
| `commit.git_failed` | `git commit` itself failed | Inspect `diagnostics` |
| `commit.result_failed` | Commit was created but its hash couldn't be read | Check `git log` directly |
| `output.unsupported` | Any command other than `commit` was asked for `--output json` | Use human mode for that command |
| `cli.invalid_arguments` | clap failed to parse the arguments | Fix the invocation |

## Human-mode commands (interactive terminal use only)

```sh
# Stage deliberately, commit with an explicit message
git add README.md src/commit.rs
git add --patch
pando commit -m "feat: add commit support"
pando commit --message "fix: preserve staged changes"

# Stage everything, with or without a generated message
pando commit --stage-all -m "chore: commit every change"
pando commit --stage-all

# Approve a shared generator before it can win
pando trust commit-status
pando trust commit-approve
```

## Introspection

```sh
pando commit --help --output json   # generated request/response JSON Schemas
```

The typed command outcome (`docs/adr/0002-render-typed-command-outcomes.md`)
is the same plan rendered two ways: the human Cliclack adapter can resolve
confirmations interactively; the JSON adapter is deterministic and
noninteractive over the identical plan.

## Commit-generation MiniJinja template

Available template variables: `git_diff`, `git_diff_stat`, `branch`,
`repo`, `recent_commits` (up to ten newest subjects). See `../config.md`
for where `command`/`template` are configured and how they layer.

```yaml
commit:
  generation:
    template: |
      Repository: {{ repo }}
      Branch: {{ branch }}
      {% for subject in recent_commits %}- {{ subject }}
      {% endfor %}
      {{ git_diff }}
```

When no template is configured, the built-in prompt requests a factual
imperative conventional-commit subject under 50 characters, a blank line,
and at least two concrete bullets. Empty generation values and invalid
YAML/templates fail before staging.

`commit.generation.command` is also the fallback generator for `pando
merge`'s default squash, so configuring it here enables squash-message
generation too. Only the `command` is shared — the squash uses its own
prompt (`merge.generation.template`, with different variables) and its own
trust namespace (`pando trust merge-approve`). Approving a shared commit
generator therefore does **not** approve it for merge. See
`lifecycle.md`.
