# worktrees

`worktrees` is a small Rust CLI for inspecting, creating, and navigating the worktrees of the Git repository containing the current directory. Git remains the source of truth: the tool calls the installed `git` executable and never fetches or maintains a repository registry.

## Requirements

- macOS or Linux
- Git
- zsh for parent-shell directory switching
- Rust 1.85 or newer when building from source

## Install

Build and install the binary as both `worktrees` and `wt`, then explicitly install the zsh integration:

```sh
just install
worktrees install
```

`just install` installs `worktrees` with Cargo and creates `wt` as a relative symlink beside it. The installer previews every mutation and asks for confirmation. It writes a commented configuration scaffold to `${XDG_CONFIG_HOME:-$HOME/.config}/worktrees/config.yaml`, writes `${XDG_CONFIG_HOME:-$HOME/.config}/worktrees/worktrees.zsh`, and adds one marked source block to `${ZDOTDIR:-$HOME}/.zshrc`. Existing configuration and shell settings are preserved, and rerunning it safely updates only the managed blocks.

Restart zsh or run:

```zsh
source ${ZDOTDIR:-$HOME}/.zshrc
```

## Structured JSON

Automation should use versioned request mode, keeping the command in argv and placing command-specific input on stdin:

```sh
printf '%s\n' '{"schema_version":1,"request_id":"example","input":{}}' \
  | worktrees list --input-output json
printf '%s\n' '{"schema_version":1,"input":{"property":"primary_worktree_path"}}' \
  | worktrees get --input-output json
```

`--output json` instead uses ordinary argv flags as input. Both modes emit exactly one newline-terminated JSON document on stdout and no ordinary stderr on typed success or failure. Requests reject unknown fields, unsupported versions, trailing data, and mixed stdin/argv command input. Paths use tagged UTF-8 or base64 objects; responses carry typed results or errors plus context, effects, bounded diagnostics, and recovery steps. Structured `list` worktrees and `switch.selection_required` choices include nullable `last_commit_at` values as RFC 3339 committer timestamps with explicit offsets. These records stay in Git order regardless of personal sort configuration; metadata lookup failures use `null` values and a bounded diagnostic.

JSON execution is deterministic and noninteractive. Mutating commands support dry-run planning, while shared trust approval and installer writes remain manual human operations. The canonical property is `primary-worktree-path` (`primary_worktree_path` in JSON); the former Main spelling is not an alias. The installed zsh wrappers for `worktrees` and `wt` pass JSON invocations and all noninteractive-shell invocations through byte-for-byte without destination capture or `cd`.

## Switching and creating

Switch directly to a branch or open the interactive picker:

```zsh
worktrees switch feature/login
worktrees switch
```

The picker lists navigable worktrees with aligned branch, last-commit, and path columns, defaults to the current worktree, and ends with **Create or switch branch…**. Last commit means the HEAD commit's committer time and is shown in local time as `YYYY-MM-DD HH:MM`, or `unknown` when it cannot be resolved. The initial order comes from `worktrees.default-sort` and falls back to Git order. Ctrl-S cycles temporarily through Git order, branch A-Z, last commit newest-first, and path A-Z while preserving the filter and selected worktree. Escape cancels without changing directory.

`worktrees switch --branches` (or `-b`) opens the picker in **branch view**: local branches instead of worktrees, including ones that have never been checked out anywhere. Ctrl-B toggles between worktree view and branch view for the current invocation only — nothing persists, and there is no configuration key for the default view. Toggling never touches Git, keeps the typed filter, and keeps the highlighted selection whenever it exists in both views; a highlighted worktree or branch with no counterpart in the other view falls back to the top of the list. Selecting an already-checked-out branch in branch view navigates to its worktree exactly as worktree view does. Selecting a branch with no worktree runs the same resolution below, creating a worktree for it.

Branch resolution is local and has no implicit fetch:

1. enter the registered worktree for an exact local branch;
2. create a worktree for an existing local branch;
3. create a local tracking branch for one same-named, already-fetched remote branch;
4. ask which remote to use when several match;
5. confirm creation of a genuinely new branch.

New branches start from the invoking worktree's committed `HEAD`, including when it is detached. Staged, unstaged, and untracked changes stay in the source worktree and produce a warning; they are not copied. Git validates names before prompting or mutation. Bare repositories may switch among existing linked worktrees, but cannot create new ones.

`worktrees create` runs the same resolution and setup, but skips step 5's confirmation:

```zsh
worktrees create feature/login
```

It reports the branch it is about to create instead of asking, so it works without a terminal, and it refuses a branch that already has a registered worktree rather than entering it. Post-create hook approval is unaffected and still requires a human. Unlike `switch`, `create` requires a branch name and has no picker.

Created worktrees use the complete branch name below the configured root, so `feature/login` becomes `<root>/feature/login`. Existing destinations and broken registered worktrees are rejected; the tool never adopts, repairs, prunes, moves, backs up, or deletes them.

The Rust binary writes only a successful destination to stdout. Setup messages and child output use stderr. The installed zsh function forwards all `switch` and `create` arguments, changes directory whenever a destination is returned, and preserves a nonzero setup status after the directory change.

## Configuration

Configuration is strict YAML: malformed files, duplicate or unknown keys, wrong types, empty commands, and empty names are errors with file context.

### Global placement

Create `${XDG_CONFIG_HOME:-$HOME/.config}/worktrees/config.yaml` manually:

```yaml
worktrees:
  root: ../worktrees
  default-sort: last-commit-at
```

The global file controls placement and the personal default sort; it cannot define hooks. `default-sort` accepts `git`, `branch`, `last-commit-at`, or `path`, and defaults to `git` when omitted. Absolute roots are used directly. Relative roots are anchored at Git's primary worktree, not the current nested directory or linked worktree.

### Shared project setup

A committed `.worktrees.yaml` in the invoking worktree may define setup, but cannot control placement or the personal `default-sort` preference:

```yaml
hooks:
  post-create:
    - name: Install dependencies
      command: npm install
    - command: echo "PORT=$(worktrees get port)" > .env.local
```

Each step runs sequentially from the new worktree root through `/bin/sh -c`. Steps inherit the ordinary environment and `PATH`, stop at the first failure, and stream both stdout and stderr to the CLI's stderr. If a hook invokes `worktrees`, the installed binary must already be on `PATH`.

### Personal per-clone overlay

A `.worktrees.local.yaml` in the primary worktree can override placement and append personal hooks:

```yaml
worktrees:
  root: /Volumes/fast/worktrees
  default-sort: path
hooks:
  post-create:
    - name: Personal editor setup
      command: ./scripts/configure-editor.sh
```

Git must report this file as ignored. Add this to the primary worktree's `.gitignore` or an applicable personal exclude file:

```gitignore
/.worktrees.local.yaml
```

Shared hooks run before local hooks. The shared file is read from the invoking worktree so setup follows the branch from which creation begins; the local overlay is always read from the primary worktree and is shared by linked worktrees. The local root and default sort override their global values.

If a configured root is inside the primary checkout, that destination must also be ignored before creation. For example, with `root: .worktrees`, add:

```gitignore
/.worktrees/
```

No configured root is an error with a copyable global configuration example. `worktrees install` adds a commented scaffold for `worktrees.root`, the optional target branch fallback, and the optional PR metadata generator to the global YAML file. The scaffold never enables a setting, preserves existing configuration, and is idempotent.

## Setup trust and recovery

Executable identity consists only of the effective ordered command strings. Before untrusted nonempty setup is allowed to create a worktree, the CLI shows every command and asks for approval. Names, comments, and formatting do not invalidate approval; adding, removing, editing, reordering, or reverting commands does.

Approval is scoped to the canonical path of this repository clone and stored atomically in `${XDG_CONFIG_HOME:-$HOME/.config}/worktrees/trust.json`. It is never shared automatically across clones and there is no noninteractive bypass.

```sh
worktrees trust status
worktrees trust reset
```

`status` distinguishes no commands, trusted commands, and untrusted commands. `reset` idempotently removes only the current clone's record, so the next creation with commands asks again.

For nonempty setup, an incomplete record is written under Git's common directory after worktree creation and before the first step. A failed step preserves the worktree, emits its destination, and returns nonzero. The zsh wrapper enters it so it can be inspected. Ctrl-C preserves the worktree and record but emits no destination, so the shell stays put.

A later switch to that worktree offers:

- **Retry setup** — resolve the invoking worktree's current shared configuration and current primary local overlay, rechecking trust if commands changed;
- **Enter once** — preserve the record, enter with a warning, and return nonzero;
- **Mark setup complete and enter** — remove the record without running setup and return success.

If the current effective hook list is empty, an obsolete record is cleared automatically.

## Committing

Bare `worktrees commit` commits only the existing Git index. Stage a deliberate selection with Git, then commit it:

```sh
git add README.md src/commit.rs       # selected paths
git add --patch                       # selected hunks
worktrees commit -m "feat: add commit support"
worktrees commit --message "fix: preserve staged changes"
```

Worktrees does not select files or hunks. To opt into staging every tracked, deleted, and untracked change, pass `--stage-all`:

```sh
worktrees commit --stage-all -m "chore: commit every change"
worktrees commit --stage-all          # use the configured generator
```

When an interactive bare commit finds a dirty worktree but an empty index, it previews the all-change candidate and offers a default-No confirmation. `--dry-run` validates and previews without staging, generating, running hooks, changing trust, or committing. Shared generation is approved separately with `worktrees trust commit-approve`.

Without a message, `worktrees commit` renders the staged snapshot into a MiniJinja prompt and sends it on stdin to a configured generator. Its stdout becomes the complete commit message; its stderr remains visible. The generator runs from the worktree root through `/bin/sh -c`. Git's normal hooks, signing, and failures remain enabled. A generator failure after `--stage-all` leaves the all-changes snapshot staged for inspection or retry.

Configure a personal generator globally:

```yaml
# ${XDG_CONFIG_HOME:-$HOME/.config}/worktrees/config.yaml
commit:
  generation:
    command: pi --no-session --no-tools
```

A committed `.worktrees.yaml` and ignored `.worktrees.local.yaml` can each also contribute `commit.generation.command` and `commit.generation.template`:

```yaml
# .worktrees.yaml (committed; requires approval if it wins)
commit:
  generation:
    template: "Use this repository's established commit style."
```

```yaml
# .worktrees.local.yaml (Git-ignored)
commit:
  generation:
    command: my-local-generator
```

Command and template resolve independently: local, then shared, then global. A local file must remain Git-ignored as described above.

The optional template has MiniJinja variables `git_diff`, `git_diff_stat`, `branch`, `repo`, and `recent_commits` (up to ten newest subjects):

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

When no template is configured, the built-in prompt requests a factual imperative conventional-commit subject under 50 characters, a blank line, and at least two concrete bullets. Empty generation values and invalid YAML or templates fail before staging.

Committed shared generator fields are untrusted executable code or model instructions. Before staging, the CLI displays only the effective shared fields and asks for default-negative, per-clone approval; there is no noninteractive bypass. User-controlled global/local values require no approval. Manage this separately from post-create-hook trust:

```sh
worktrees trust commit-status
worktrees trust commit-approve
worktrees trust commit-reset
```

`commit-approve` is interactive, default-negative, and records trust without staging or committing. `commit-reset` is idempotent and does not alter post-create approval. Supplying `-m`/`--message` bypasses all generator configuration, template validation, and generator trust.

### Structured commit I/O

Use ordinary argv with a structured response, or a strict versioned request on stdin:

```sh
worktrees commit --dry-run -m "fix: preview" --output json
printf '%s\n' '{"schema_version":1,"request_id":"job-42","input":{"selection":"staged","message":{"source":"provided","value":"fix: preserve index"},"dry_run":false}}' \
  | worktrees commit --input-output json
worktrees commit --help --output json   # generated request/response schemas and catalogs
worktrees --help --output json          # command support index
```

JSON mode emits exactly one document on stdout and nothing on stderr. Errors are nonzero and include stable codes and typed `next_steps`; for example, `commit.nothing_staged` suggests `git.stage_paths`, `git.stage_patch`, and `commit.stage_all`. JSON requests cannot approve shared generators.

## Lifecycle commands

`worktrees remove [--force] [branch ...]` removes registered topic worktrees but never deletes their local branch refs. No arguments removes the current topic; explicit branches select registered topics. Dirty worktrees require `--force`, and removing the current worktree emits the primary path after deletion.

`worktrees pr create` requires `pr.generation.command` only when `--title` or `--description` is omitted. Supplying both explicit values bypasses generator configuration and trust checks. Missing generator configuration is rejected before any dirty-worktree commit, skip, or yolo handling. When no target branch is configured, PR and merge operations fall back to the fetched `origin/HEAD`, then local `main`, then local `master`.

`worktrees merge [--no-rebase] [--no-remove] [--yolo]` integrates the current clean topic into the configured target checked out in the primary worktree using `git merge --ff-only`. When no target is configured, it falls back to the already-fetched `origin/HEAD` branch, then local `main`, then local `master`, without fetching. A diverged topic rebases by default. `--yolo` first runs the equivalent of `worktrees commit --stage-all`, using the configured commit-message generator, and then merges if the commit succeeds. It is available only with human output and cannot be combined with `--dry-run`. Phase-specific `pre-merge` and `pre-remove` hooks run at their lifecycle boundaries; the journal pins recovery state through conflicts and cleanup retries.

## Context queries

Every successful `get` command writes exactly one value and a newline:

```sh
branch=$(worktrees get branch)
path=$(worktrees get worktree-path)
main=$(worktrees get primary-worktree-path)
root=$(worktrees get worktree-root)
port=$(worktrees get port)
```

- `branch` — full named branch of the containing worktree;
- `worktree-path` — resolved absolute containing worktree root;
- `primary-worktree-path` — resolved absolute primary worktree path;
- `worktree-root` — resolved absolute effective configured creation root;
- `port` — deterministic branch-only port in `10000..=19999`, compatible with Worktrunk v0.66.0.

Queries work from nested directories. Branch-dependent properties fail in detached worktrees, and `worktree-root` fails when no root is configured.

## Listing

```sh
worktrees list
```

The aligned table shows `BRANCH`, `LAST COMMIT AT`, and `PATH`, plus the current `*` marker, dirty state, and exceptional detached, bare, locked, prunable, missing, inaccessible, or unknown states. Last-commit values use each HEAD commit's committer timestamp in local `YYYY-MM-DD HH:MM` form, or `unknown` when metadata is unavailable. The active branch, last-commit, or path sort has a direction arrow; Git order is named in the heading. Human ordering follows `worktrees.default-sort`, while structured JSON always preserves Git's discovery order.

```sh
worktrees list --branches
```

`worktrees list --branches` (or `-b`) lists local branches (`refs/heads`) instead of worktrees, so a branch that has never been checked out anywhere is still visible. Remote-tracking branches never appear and no fetch happens. The `PATH` cell is blank for a branch with no worktree, and an unattached branch shows no condition marker — there is no working tree to call clean or dirty. Detached and bare worktrees have no branch and so are absent from this view; it is not a superset of `worktrees list`. `worktrees list --branches --output json` emits a distinct `branches` payload, always in `for-each-ref` order.

## `wt` short name

This release does **not** install a `wt` command or alias because Worktrunk may own that name. The generated zsh integration contains a commented example that can be enabled manually after resolving that conflict.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

Behavioral tests use real temporary Git repositories, pseudo-terminals, isolated shell homes, and zsh where available.

## License

MIT
