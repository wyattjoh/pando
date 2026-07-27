# `install`

```
Usage: worktrees install [OPTIONS]
```

No flags beyond the globals. Installs the managed zsh integration:

- Previews every mutation and asks for confirmation before writing anything.
- Writes `${XDG_CONFIG_HOME:-$HOME/.config}/worktrees/worktrees.zsh`.
- Adds one marked source block to `${ZDOTDIR:-$HOME}/.zshrc`.
- Rerunning it safely updates the managed function without duplicating the
  source block.

After installing, restart zsh or:

```zsh
source ${ZDOTDIR:-$HOME}/.zshrc
```

Note: this release does **not** install a `wt` command/alias (the name may
be owned by Worktrunk); the generated zsh integration file has a commented
example that can be enabled manually.

The installed function wraps `switch`/`remove`/`merge` so it can `cd` the
parent shell to the destination path the binary prints on stdout; all other
subcommands pass straight through to the real binary. `command worktrees
...` always bypasses the wrapper.

Both JSON modes are supported; agents use versioned request mode.

```sh
cargo install --path .
worktrees install
source ${ZDOTDIR:-$HOME}/.zshrc
```

## Structured JSON contract

`install --input-output json` accepts `input.dry_run`. Dry run performs path/content preflight, prompts for nothing, writes nothing, and returns planned `file.write` effects with byte-safe targets. An already-current installation has its own successful outcome. A non-dry JSON request requiring writes returns `install.approval_required`, planned effects, and a manual `worktrees install` next step. Human `--dry-run` renders the same plan without JSON or mutation. The zsh function detects JSON flags by argv element (including `--flag=json`) and bypasses destination capture in JSON or noninteractive shells.
