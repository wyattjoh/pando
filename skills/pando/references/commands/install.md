# `install`

```
Usage: pando install [OPTIONS]
```

Installs the managed zsh integration and then starts LLM-guided configuration:

- Previews every deterministic shell mutation and asks for confirmation before
  writing anything.
- Adds a commented scaffold to `${XDG_CONFIG_HOME:-$HOME/.config}/pando/config.yaml`.
  It leaves `worktrees.root` commented for required placement configuration,
  documents the optional target-branch fallback, and documents the optional PR
  metadata generator used when PR metadata is omitted.
- Writes `${XDG_CONFIG_HOME:-$HOME/.config}/pando/pando.zsh`.
- Adds one marked source block to `${ZDOTDIR:-$HOME}/.zshrc`.
- Preserves existing configuration and shell content. Rerunning it safely
  updates only managed content and is idempotent.
- Detects installed Pi, Claude Code, Codex, and Gemini CLI commands, then shows
  a selector whose first choice is a previously saved command when present.
- Offers the selected command as editable input, saves it in a managed global
  `install.command` block, and launches it with an initial configuration prompt.
- Keeps the child agent's stdin and terminal output interactive while routing
  its stdout to Pando's stderr stream, preserving empty Pando stdout.

`--no-guide` skips agent selection and performs only the deterministic shell
installation. `--dry-run` previews that shell installation without prompting,
writing, saving a command, or launching an agent.

After installing, restart zsh or:

```zsh
source ${ZDOTDIR:-$HOME}/.zshrc
```

The generated zsh integration does not define a `pd` shell alias. Installing
from this checkout with `just install` creates `pd` as a relative symlink
beside the installed `pando` executable, then the integration defines
`pando` and `pd` wrapper functions that invoke their corresponding binaries.

Both installed functions wrap `switch`/`remove`/`merge` so they can `cd` the
parent shell to the destination path the selected binary prints on stdout;
all other subcommands pass straight through. `command pando ...` and
`command pd ...` always bypass the wrappers.

Both JSON modes are supported; agents use versioned request mode.

```sh
just install
pando install
source ${ZDOTDIR:-$HOME}/.zshrc
```

## Tab completion

Installation also registers zsh tab completion for both `pando` and `pd`.

Completion is dynamic: zsh calls the binary on each Tab, so suggestions always
reflect the current repository. It covers subcommands, flags, and value enums
(`--output`, `pando get <property>`), plus branch arguments, filtered per
command:

| Command | Offers |
|---|---|
| `switch <branch>` | every local branch, plus remote-tracking refs no local branch shadows |
| `create <branch>` | branches that do not already have a worktree |
| `remove <branches>...` | only branches with a removable non-primary worktree, annotated with the worktree path |

Outside a Git repository, or when Git fails, completion offers nothing rather
than reporting an error.

Two limitations: `remove` re-offers branches already typed on the command line,
and only zsh is supported. Existing installations pick up completion by
re-running `pando install` and starting a new shell.

## Guided agent contract

Pando invokes the editable command through `/bin/sh -c` and appends one quoted
initial prompt as a positional argument. Pi, Claude Code, Codex, and Gemini CLI
all retain their interactive session in this form. The user-entered command is
therefore an explicitly authorized shell command and may include agent flags.
A command containing shell operators must include one `{prompt}` placeholder
where that argument belongs, for example `claude {prompt} | tee transcript.log`.
The prompt tells the agent to preserve Pando's managed marker blocks, keep the
saved `install.command`, ask before edits, validate strict YAML, and cover the
full global configuration surface. Project `.pando.yaml` or
`.pando.local.yaml` edits require separate explicit approval inside the agent
session.

## Structured JSON contract

`install --input-output json` accepts `input.dry_run`. Dry run performs path/content preflight, prompts for nothing, writes nothing, launches no agent, and returns planned `file.write` effects with byte-safe targets. An already-current deterministic installation has its own successful outcome. A non-dry JSON request requiring writes returns `install.approval_required`, planned effects, and a manual `pando install` next step, which uses guided mode by default. Human `--dry-run` renders the same deterministic plan without JSON or mutation. `--no-guide` is an argv-only human option and is rejected when mixed with `--input-output json`. The zsh function detects JSON flags by argv element (including `--flag=json`) and bypasses destination capture in JSON or noninteractive shells.
