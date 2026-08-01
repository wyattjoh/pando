# Config format — three-layer YAML

All three files are strict YAML (`deny_unknown_fields` on every struct):
malformed files, duplicate/unknown keys, wrong types, empty `command`
strings, and empty `name` strings are all hard errors with file context.

| Layer | Path | Can set placement (`worktrees.root`) | Can set `worktrees.default-sort` | Can set `worktrees.base` | Can set hooks | Can set `commit.generation.*` |
|---|---|---|---|---|---|---|
| Global | `${XDG_CONFIG_HOME:-$HOME/.config}/pando/config.yaml` | yes | yes | yes | **no** | yes |
| Shared | `.pando.yaml` in the **invoking** worktree | **no** (only `target-branch` and `base`) | **no** | yes | yes | yes (untrusted — needs `pando trust commit-approve`) |
| Local | `.pando.local.yaml` in the **primary** worktree | yes | yes | yes | yes | yes |

`pando install` adds a commented scaffold to the global YAML file. It
never enables a setting or edits existing user configuration. The scaffold is
idempotent and documents the required placement root, the optional target
branch fallback, and the optional PR metadata generator.

## 1. Global placement

```yaml
# ${XDG_CONFIG_HOME:-$HOME/.config}/pando/config.yaml
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

For PR creation, `pr.generation.command` is required only when the title or
description is omitted. Supplying both values explicitly bypasses this
configuration. `pando install` includes this as commented guidance:

```yaml
# pr:
#   generation:
#     command: pi --no-session --no-tools
```

## 2. Shared project setup

```yaml
# .pando.yaml (committed; read from the invoking worktree)
hooks:
  post-create:
    - name: Install dependencies
      command: npm install
    - command: echo "PORT=$(pando get port)" > .env.local
```

Each hook step runs sequentially from the new worktree root through
`/bin/sh -c`, inherits the ordinary environment/`PATH`, stops at the first
failure, and streams stdout+stderr to the CLI's stderr. If a hook invokes
`pando` itself, the binary must be on `PATH`.

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

`target-branch` overrides the default target for `pando merge` and PR
creation. When it is omitted, operations fall back to the local branch pointed
to by `origin/HEAD`, then local `main`, then local `master`. Set it in
`.pando.yaml` or the global config when a different target is needed.

May also set where new branches are cut from — the one placement-adjacent
key legal in all three layers:

```yaml
worktrees:
  base: fresh
```

## New-branch base

`worktrees.base` accepts only `head` or `fresh`; anything else is a hard
error naming the file. It resolves local, then shared, then global, and
defaults to `head`.

| Value | Where a genuinely new branch starts |
|---|---|
| `head` (default) | The invoking worktree's committed `HEAD` — today's behavior |
| `fresh` | The remote-tracking ref of `target-branch`, or of the branch named by the remote's `origin/HEAD` when no target branch is set |

`fresh` reads local refs only. If neither `target-branch` nor `origin/HEAD`
names a branch, or the resolved `origin/<branch>` was never fetched into the
clone, creation fails with guidance (`git fetch`,
`git remote set-head origin -a`, or configure `target-branch`) rather than
branching from the wrong point. `pando switch --fetch` and
`pando create --fetch` refresh exactly that one base ref first; see
`commands/switch.md`. The base only changes the start point of a genuinely
new branch — resolution of existing worktrees, local branches, and remote
matches is unchanged.

## 3. Personal per-clone overlay

```yaml
# .pando.local.yaml (in the primary worktree)
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
/.pando.local.yaml
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

## Pull-request templates

`pr.pull-request-template` is a body-template value, separate from the
`pr.generation.template` prompt. Its precedence is local configuration,
shared project configuration, the committed repository template, then global
configuration. Repository lookup prefers `.github/pull_request_template.md`,
then `.github/PULL_REQUEST_TEMPLATE.md` (with root-level casing fallbacks),
and reads only from committed `HEAD`. The resolved value is exposed to the
MiniJinja prompt as `pull_request_template`; a custom generation prompt must
place it explicitly, while the built-in prompt always includes it.

```yaml
pr:
  pull-request-template: |
    ## Summary
    ## Testing
```

## Layering rules

- **Hooks**: shared (`.pando.yaml`) steps run before local
  (`.pando.local.yaml`) steps, concatenated per phase.
- **Root**: local root overrides global root. There is no intermediate
  "shared root" — the shared file cannot set `worktrees.root` at all, only
  `worktrees.target-branch` and `worktrees.base`.
- **Base**: local, then shared, then global — the only `pando` key that
  layers through all three files.
- **Default sort**: the ignored local value overrides the global value. The
  committed shared file cannot set this personal interface preference.
- **Commit generation** (`command` and `template` resolve independently):
  local, then shared, then global — first layer that sets the value wins.

## Commit-generation MiniJinja template

Available variables: `git_diff`, `git_diff_stat`, `branch`, `repo`,
`recent_commits` (up to ten newest subjects). See
`commands/commit.md` for the full behavior and a worked template.
