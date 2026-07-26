# worktrees

`worktrees` is a small Rust CLI for inspecting, creating, and navigating the worktrees of the Git repository containing the current directory. Git remains the source of truth: the tool calls the installed `git` executable and never fetches or maintains a repository registry.

## Requirements

- macOS or Linux
- Git
- zsh for parent-shell directory switching
- Rust 1.85 or newer when building from source

## Install

Build and install the binary, then explicitly install the zsh integration:

```sh
cargo install --path .
worktrees install
```

The installer previews every mutation and asks for confirmation. It writes `${XDG_CONFIG_HOME:-$HOME/.config}/worktrees/worktrees.zsh` and adds one marked source block to `${ZDOTDIR:-$HOME}/.zshrc`. Rerunning it safely updates the managed function without duplicating the source block.

Restart zsh or run:

```zsh
source ${ZDOTDIR:-$HOME}/.zshrc
```

## Switching and creating

Switch directly to a branch or open the interactive picker:

```zsh
worktrees switch feature/login
worktrees switch
```

The picker lists navigable worktrees in Git's order, defaults to the current worktree, and ends with **Create or switch branch…**. Escape cancels without changing directory.

Branch resolution is local and has no implicit fetch:

1. enter the registered worktree for an exact local branch;
2. create a worktree for an existing local branch;
3. create a local tracking branch for one same-named, already-fetched remote branch;
4. ask which remote to use when several match;
5. confirm creation of a genuinely new branch.

New branches start from the invoking worktree's committed `HEAD`, including when it is detached. Staged, unstaged, and untracked changes stay in the source worktree and produce a warning; they are not copied. Git validates names before prompting or mutation. Bare repositories may switch among existing linked worktrees, but cannot create new ones.

Created worktrees use the complete branch name below the configured root, so `feature/login` becomes `<root>/feature/login`. Existing destinations and broken registered worktrees are rejected; the tool never adopts, repairs, prunes, moves, backs up, or deletes them.

The Rust binary writes only a successful destination to stdout. Setup messages and child output use stderr. The installed zsh function forwards all `switch` arguments, changes directory whenever a destination is returned, and preserves a nonzero setup status after the directory change.

## Configuration

Configuration is strict YAML: malformed files, duplicate or unknown keys, wrong types, empty commands, and empty names are errors with file context.

### Global placement

Create `${XDG_CONFIG_HOME:-$HOME/.config}/worktrees/config.yaml` manually:

```yaml
worktrees:
  root: ../worktrees
```

The global file controls only placement; it cannot define hooks. Absolute roots are used directly. Relative roots are anchored at Git's primary worktree, not the current nested directory or linked worktree.

### Shared project setup

A committed `.worktrees.yaml` in the invoking worktree may define setup, but cannot control placement:

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
hooks:
  post-create:
    - name: Personal editor setup
      command: ./scripts/configure-editor.sh
```

Git must report this file as ignored. Add this to the primary worktree's `.gitignore` or an applicable personal exclude file:

```gitignore
/.worktrees.local.yaml
```

Shared hooks run before local hooks. The shared file is read from the invoking worktree so setup follows the branch from which creation begins; the local overlay is always read from the primary worktree and is shared by linked worktrees. The local root overrides the global root.

If a configured root is inside the primary checkout, that destination must also be ignored before creation. For example, with `root: .worktrees`, add:

```gitignore
/.worktrees/
```

No configured root is an error with a copyable global configuration example; `worktrees install` does not create or edit YAML.

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

Stage every tracked, deleted, and untracked change and commit it directly when the message is known:

```sh
worktrees commit -m "feat: add commit support"
worktrees commit --message "fix: preserve staged changes"
```

Without a message, `worktrees commit` renders the staged snapshot into a MiniJinja prompt and sends it on stdin to a configured generator. Its stdout becomes the complete commit message; its stderr remains visible. The generator runs from the worktree root through `/bin/sh -c`. Git's normal commit output, hooks, signing, and failures are forwarded unchanged. A generator failure leaves the all-changes snapshot staged for inspection or retry.

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
worktrees trust commit-reset
```

`commit-reset` is idempotent and does not alter post-create approval. Supplying `-m`/`--message` bypasses all generator configuration, template validation, and generator trust.

## Context queries

Every successful `get` command writes exactly one value and a newline:

```sh
branch=$(worktrees get branch)
path=$(worktrees get worktree-path)
main=$(worktrees get main-worktree-path)
root=$(worktrees get worktree-root)
port=$(worktrees get port)
```

- `branch` — full named branch of the containing worktree;
- `worktree-path` — resolved absolute containing worktree root;
- `main-worktree-path` — resolved absolute primary worktree path;
- `worktree-root` — resolved absolute effective configured creation root;
- `port` — deterministic branch-only port in `10000..=19999`, compatible with Worktrunk v0.66.0.

Queries work from nested directories. Branch-dependent properties fail in detached worktrees, and `worktree-root` fails when no root is configured.

## Listing

```sh
worktrees list
```

The aligned table includes Git's worktree order, branch, absolute path, current `*` marker, dirty state, and exceptional detached, bare, locked, prunable, missing, inaccessible, or unknown states.

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
