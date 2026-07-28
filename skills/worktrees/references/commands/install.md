# `install`

```
Usage: worktrees install [OPTIONS]
```

No flags beyond the globals. Installs the managed zsh integration and global
configuration scaffold:

- Previews every mutation and asks for confirmation before writing anything.
- Adds a commented scaffold to `${XDG_CONFIG_HOME:-$HOME/.config}/worktrees/config.yaml`.
  It leaves `worktrees.root` commented for required placement configuration,
  documents the optional target-branch fallback, and documents the optional PR
  metadata generator used when PR metadata is omitted.
- Writes `${XDG_CONFIG_HOME:-$HOME/.config}/worktrees/worktrees.zsh`.
- Adds one marked source block to `${ZDOTDIR:-$HOME}/.zshrc`.
- Preserves existing configuration and shell content. Rerunning it safely
  updates only managed content and is idempotent.

After installing, restart zsh or:

```zsh
source ${ZDOTDIR:-$HOME}/.zshrc
```

The generated zsh integration does not define a `wt` shell alias. Installing
from this checkout with `just install` creates `wt` as a relative symlink
beside the installed `worktrees` executable, then the integration defines
`worktrees` and `wt` wrapper functions that invoke their corresponding binaries.

Both installed functions wrap `switch`/`remove`/`merge` so they can `cd` the
parent shell to the destination path the selected binary prints on stdout;
all other subcommands pass straight through. `command worktrees ...` and
`command wt ...` always bypass the wrappers.

Both JSON modes are supported; agents use versioned request mode.

```sh
just install
worktrees install
source ${ZDOTDIR:-$HOME}/.zshrc
```

## Structured JSON contract

`install --input-output json` accepts `input.dry_run`. Dry run performs path/content preflight, prompts for nothing, writes nothing, and returns planned `file.write` effects with byte-safe targets. An already-current installation has its own successful outcome. A non-dry JSON request requiring writes returns `install.approval_required`, planned effects, and a manual `worktrees install` next step. Human `--dry-run` renders the same plan without JSON or mutation. The zsh function detects JSON flags by argv element (including `--flag=json`) and bypasses destination capture in JSON or noninteractive shells.
