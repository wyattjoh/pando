# `commit`

**Never run `git commit` directly in a repo that uses `worktrees`.** The
commit itself — staged-only or `--stage-all` — must go through `worktrees
commit`. If the caller didn't give you an exact commit message, don't
compose one yourself and pass it via `-m`: omit `-m` entirely and let
`worktrees commit` invoke the configured generator (or its own built-in
prompt if none is configured).

```
Usage: worktrees commit [OPTIONS]
```

| Flag | Purpose |
|---|---|
| `-m`, `--message <MESSAGE>` | Commit message. Omit to use the configured generator. Supplying it bypasses all generator configuration, template validation, and generator trust |
| `--stage-all` | Stage tracked, deleted, and untracked changes before committing |
| `--dry-run` | Validate and preview without staging, generating, running hooks, changing trust, or committing |

`commit` is the **only** command that supports `--output json` /
`--input-output json` — it is the first (and so far only) adapter-backed
command (`docs/adr/0002-render-typed-command-outcomes.md`).

## Behavior

- **Bare `worktrees commit`** commits only the existing Git index. Stage
  deliberately first with `git add <paths>` or `git add --patch`, then run
  `worktrees commit` (no `-m`, so the configured generator writes the
  message) or `worktrees commit -m "..."` only if you were given that exact
  message.
- **`--stage-all`** opts into staging every tracked/deleted/untracked
  change before committing.
- An interactive bare commit that finds a dirty worktree but an **empty**
  index previews the all-change candidate and offers a default-No
  confirmation before staging everything.
- **Without `-m`**, the staged snapshot is rendered into a MiniJinja prompt
  sent on stdin to the configured generator command (`/bin/sh -c`, run from
  the worktree root, ordinary environment/`PATH`). The generator's stdout
  becomes the commit message; its stderr stays visible. Git's normal hooks,
  signing, and failure behavior remain enabled. A generator failure after
  `--stage-all` leaves the all-changes snapshot staged for inspection or
  retry.
- Shared (`.worktrees.yaml`) generator fields (`command`/`template`) are
  **untrusted** and must be approved via `worktrees trust commit-approve`
  (default-negative, interactive, no noninteractive bypass) before they can
  win. User global/local generator values need no approval.

## Commands

```sh
# Stage deliberately, commit with an explicit message
git add README.md src/commit.rs
git add --patch
worktrees commit -m "feat: add commit support"
worktrees commit --message "fix: preserve staged changes"

# Stage everything, with or without a generated message
worktrees commit --stage-all -m "chore: commit every change"
worktrees commit --stage-all

# Approve a shared generator before it can win
worktrees trust commit-status
worktrees trust commit-approve
```

## JSON request/response contract

JSON mode emits exactly one document on stdout, nothing on stderr. Errors
are nonzero exit codes with stable error codes and typed `next_steps` (e.g.
`commit.nothing_staged` suggests `git.stage_paths`, `git.stage_patch`,
`commit.stage_all`). **JSON requests cannot approve shared generators** —
that trust step must happen interactively first.

```sh
# One-shot: flags in, JSON out
worktrees commit --dry-run -m "fix: preview" --output json

# Full request/response: JSON in via --input-output json
printf '%s\n' '{"schema_version":1,"request_id":"job-42","input":{"selection":"staged","message":{"source":"provided","value":"fix: preserve index"},"dry_run":false}}' \
  | worktrees commit --input-output json

# Generated JSON Schemas for the request/response envelope
worktrees commit --help --output json
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
