# Config format — three-layer YAML

All three files are strict YAML (`deny_unknown_fields` on every struct):
malformed files, duplicate/unknown keys, wrong types, empty `command`
strings, and empty `name` strings are all hard errors with file context.

| Layer | Path | Can set placement (`worktrees.root`) | Can set `worktrees.default-sort` | Can set hooks | Can set `commit.generation.*` |
|---|---|---|---|---|---|
| Global | `${XDG_CONFIG_HOME:-$HOME/.config}/worktrees/config.yaml` | yes | yes | **no** | yes |
| Shared | `.worktrees.yaml` in the **invoking** worktree | **no** (only `target-branch`) | **no** | yes | yes (untrusted — needs `worktrees trust commit-approve`) |
| Local | `.worktrees.local.yaml` in the **primary** worktree | yes | yes | yes | yes |

`worktrees install` never creates or edits any of these YAML files — you
write them by hand.

## 1. Global placement

```yaml
# ${XDG_CONFIG_HOME:-$HOME/.config}/worktrees/config.yaml
worktrees:
  root: ../worktrees
  default-sort: last-commit-at
```

`default-sort` accepts only `git`, `branch`, `last-commit-at`, or `path`.
It controls the initial human `list` and interactive `switch` order, defaults
to `git` when omitted, and never changes structured JSON ordering. Absolute
roots are used directly; relative roots are anchored at Git's
**primary** worktree, never the current nested/linked worktree. No
configured root at all is an error (across all layers) when `switch` needs
to create a worktree.

May also set the commit generator globally:

```yaml
commit:
  generation:
    command: pi --no-session --no-tools
```

## 2. Shared project setup

```yaml
# .worktrees.yaml (committed; read from the invoking worktree)
hooks:
  post-create:
    - name: Install dependencies
      command: npm install
    - command: echo "PORT=$(worktrees get port)" > .env.local
```

Each hook step runs sequentially from the new worktree root through
`/bin/sh -c`, inherits the ordinary environment/`PATH`, stops at the first
failure, and streams stdout+stderr to the CLI's stderr. If a hook invokes
`worktrees` itself, the binary must be on `PATH`.

Hook phase keys: `post-create`, `pre-merge`, `pre-remove`.

May contribute a commit-generation template (untrusted — needs approval if
it wins):

```yaml
commit:
  generation:
    template: "Use this repository's established commit style."
```

May also set the merge target branch (the only placement-adjacent key
available at this layer):

```yaml
worktrees:
  target-branch: main
```

`target-branch` is required for `worktrees merge` and is **not documented
in README.md's Configuration section** — it only surfaces via the
`require_target_branch` error message. Set it in `.worktrees.yaml` or the
global config.

## 3. Personal per-clone overlay

```yaml
# .worktrees.local.yaml (in the primary worktree)
worktrees:
  root: /Volumes/fast/worktrees
  default-sort: path
hooks:
  post-create:
    - name: Personal editor setup
      command: ./scripts/configure-editor.sh
```

**Must be Git-ignored**, or `EffectiveConfig::load` hard-errors:

```gitignore
/.worktrees.local.yaml
```

May also contribute a generator command:

```yaml
commit:
  generation:
    command: my-local-generator
```

If a configured root lands inside the primary checkout (e.g.
`root: .worktrees`), that destination must also be Git-ignored before
creation:

```gitignore
/.worktrees/
```

## Layering rules

- **Hooks**: shared (`.worktrees.yaml`) steps run before local
  (`.worktrees.local.yaml`) steps, concatenated per phase.
- **Root**: local root overrides global root. There is no intermediate
  "shared root" — the shared file cannot set `worktrees.root` at all, only
  `worktrees.target-branch`.
- **Default sort**: the ignored local value overrides the global value. The
  committed shared file cannot set this personal interface preference.
- **Commit generation** (`command` and `template` resolve independently):
  local, then shared, then global — first layer that sets the value wins.

## Commit-generation MiniJinja template

Available variables: `git_diff`, `git_diff_stat`, `branch`, `repo`,
`recent_commits` (up to ten newest subjects). See
`commands/commit.md` for the full behavior and a worked template.
