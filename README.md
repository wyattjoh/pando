# worktrees

`worktrees` is a small Rust CLI for inspecting and navigating the worktrees of the Git repository containing your current directory. Git remains the source of truth: the tool calls the installed `git` executable and does not maintain a repository registry.

## Requirements

- macOS or Linux
- Git
- zsh for shell directory switching
- Rust 1.85 or newer when building from source

## Install

Build and install the binary, then explicitly install the zsh integration:

```sh
cargo install --path .
worktrees install
```

The installer previews every planned mutation and asks for confirmation. It writes managed integration code to:

```text
${XDG_CONFIG_HOME:-$HOME/.config}/worktrees/worktrees.zsh
```

It also adds one marked source block to `${ZDOTDIR:-$HOME}/.zshrc`, preserving all unrelated content. Restart zsh or load the updated startup file:

```zsh
source ${ZDOTDIR:-$HOME}/.zshrc
```

Rerunning `worktrees install` is safe and idempotent.

## Commands

### List worktrees

```sh
worktrees list
```

The aligned table includes Git's worktree order, branch, absolute path, the current `*` marker, dirty state, and exceptional states such as detached, bare, locked, prunable, missing, inaccessible, or unknown.

### Switch worktrees

```zsh
worktrees switch
```

Use the arrow keys and Enter to choose an accessible worktree; Escape cancels. After shell integration is installed, the `worktrees` zsh function changes the invoking shell's directory. Without the integration, the Rust command writes only the selected absolute path to stdout so another shell integration can consume it safely.

### Update shell integration

```sh
worktrees install
```

The installer updates only its generated integration file and marked `.zshrc` block. If both are current, it exits without prompting or modifying files.

## `wt` short name

This release does **not** install a `wt` command or alias because Worktrunk may already own that name. The generated zsh integration contains a commented example that can be enabled manually after resolving that conflict.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

The behavioral tests use real temporary Git repositories, pseudo-terminals, isolated shell homes, and zsh where available.

## License

MIT
