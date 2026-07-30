# AGENTS.md

Guidance for coding agents working in this repository. `CLAUDE.md` is a symlink to this file.

## Commands

```sh
just build            # cargo build --all-features
just test             # cargo test --all-features
just install          # cargo install --path .

cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings

cargo test --test cli switch_creates          # one integration test (substring match)
cargo test --lib                              # unit tests only (in-module #[cfg(test)])
cargo test --test cli -- --nocapture          # see PTY/child output
```

CI runs `fmt --check`, `clippy -D warnings`, and `test --all-features` on ubuntu-latest and macos-latest. Clippy `pedantic` is `warn` in `Cargo.toml` but denied in CI, so public fallible functions need `/// # Errors` doc sections and public types need `#[must_use]` where applicable.

Edition 2024, MSRV 1.85. Unix-only: the code uses `std::os::unix` APIs directly (`OsStrExt`, `ExitStatusExt`, `/bin/sh`, `/dev/stderr`).

## Architecture

`worktrees` is a CLI for inspecting, creating, and navigating the Git worktrees of the repository containing the current directory. **Git is the source of truth**: every repository fact comes from invoking the installed `git` executable (no libgit2, no cached registry, no implicit fetch).

`src/main.rs` is a thin clap dispatcher over `src/lib.rs`; all logic lives in the library so integration tests and future callers can reach it.

### Module map

| Module | Responsibility |
|---|---|
| `lib.rs` | `Worktree`/`WorktreeKind`/`Condition`/`SortMode` domain types, display/navigability rules, and stable worktree sorting |
| `git.rs` | Every `git` subprocess call; parses `worktree list --porcelain -z`; batches HEAD committer metadata; `Repository` context |
| `config.rs` | Strict three-layer YAML config parsing and effective value resolution |
| `smart.rs` | Command implementations for `switch`/`create`/`get`/`trust`; all interactive prompts |
| `trust.rs` | Post-create hook approval: command hashing, XDG `trust.json`, atomic writes |
| `setup.rs` | Post-create hook execution and the incomplete-setup journal |
| `install.rs` | Managed zsh integration; marker-block rewriting of `.zshrc` |
| `render.rs` | Column alignment shared by `list` output and the picker's menu labels, plus the shared styling for captured Git output |
| `hash.rs` | Hex encoding shared by `trust.rs` and `setup.rs` |

### Load-bearing invariants

**stdout purity.** The binary writes *only* a successful destination path (`switch`) or a single property value (`get`) to stdout, always with a trailing newline. Prompts, warnings, and hook output go to stderr. The installed zsh function captures stdout and `cd`s to it, so any stray `println!` in a `switch` path silently breaks directory switching. Note the asymmetry: `finish_setup` writes the destination *before* returning an error on hook failure, so the shell still enters a half-configured worktree while preserving the nonzero status.

**Paths are bytes, not strings.** Destinations are written with `write_all(path.as_os_str().as_bytes())` rather than `Display`, and porcelain parsing goes through `OsString::from_vec`. Non-UTF-8 and space-containing paths are covered by tests — don't introduce `to_string_lossy` on a path that reaches stdout.

**Config layering** (`EffectiveConfig::load`) is deliberately asymmetric:

- global `$XDG_CONFIG_HOME/worktrees/config.yaml` controls placement and the personal default sort, never hooks;
- `.worktrees.yaml` read from the **invoking** worktree controls shared hooks and the target branch, never placement or personal sort, so setup follows the branch you create from;
- `.worktrees.local.yaml` read from the **primary** worktree controls personal placement, hooks, and default sort, and must be Git-ignored or loading is a hard error.

Shared hooks run before local hooks; the local root and default sort override their global values. Human list and picker rendering use the effective sort, while structured JSON always retains Git discovery order. All structs use `deny_unknown_fields`, so config mistakes fail loudly with file context.

**Trust identity is command strings only.** `trust::command_hash` digests a domain-separated, length-prefixed concatenation of the ordered `command` fields — names, comments, and formatting are excluded on purpose, so reordering or editing a command revokes approval but renaming a step does not. Approval is keyed by the hex-encoded canonical primary-worktree path and there is no non-interactive bypass; `ensure_interactive` refuses rather than assuming yes.

**The incomplete-setup journal is two-phase** because a worktree's stable identity doesn't exist until Git creates it. `setup::prepare` writes a *pending* record keyed by branch hash **before** the `git worktree add`; after creation, `PendingRecord::commit` renames it to a *marker* keyed by the hash of the new worktree's git dir. Both live under `<common-dir>/worktrees-state/`. `is_incomplete`/`clear` check both locations, which is why `branch` is threaded through as an `Option`. Creation failure calls `cancel()`. Writes go through `trust::write_atomic` (temp file + `sync_all` + `rename`).

**Port hashing is pinned for compatibility.** `smart::port_for_branch` uses `SipHasher13` explicitly rather than `DefaultHasher` so a future std change cannot move ports away from Worktrunk v0.66.0. Golden values are asserted in `smart.rs` tests — treat them as a compatibility contract.

**Branch resolution order** in `resolve_and_switch`: existing registered worktree → existing local branch → single already-fetched remote match → prompt among multiple remotes → confirm a genuinely new branch from the invoking `HEAD`. The tool never adopts, repairs, prunes, moves, or deletes an existing destination or a broken worktree record.

**`switch` and `create` share one resolver, parameterized by `Intent`** — in `smart.rs` for humans and `machine.rs` for JSON. `Intent::Create` skips only the genuinely-new-branch confirmation and refuses an already-registered branch; remote selection and post-create hook trust still prompt, and `create` is the sole JSON entry point allowed to create a branch unattended (`switch` still answers `switch.approval_required`). Anything `create` changes in the shared path changes `switch` too, so keep the intent checks narrow.

### Testing

`tests/cli.rs` is the bulk of the suite and is behavioral end-to-end: it builds real temporary Git repositories, runs the compiled binary via `assert_cmd`, and drives interactive flows through a real PTY (`nix::pty::openpty`) so `IsTerminal` checks behave as they do in a shell. Tests isolate `HOME`/`XDG_CONFIG_HOME`/`ZDOTDIR` into temp dirs, and the zsh integration tests spawn actual `zsh`. Unit tests stay in-module for pure logic (porcelain parsing, `.zshrc` block rewriting, port goldens).

When adding a feature, prefer an integration test that asserts the stdout/stderr split and exit status, not just the happy path.

## Agent skills

### Issue tracker

Issues and specs are tracked as local markdown under `.scratch/<feature>/`. See `docs/agents/issue-tracker.md`.

### Domain docs

This repository uses a single-context domain-doc layout. See `docs/agents/domain.md`.

### CLI usage skill

`skills/worktrees/` documents `worktrees`' own command surface, flags, config schema, and JSON contract for kickstarting usage (symlinked into `.claude/skills/worktrees` and `.agents/skills/worktrees`). Whenever a change touches the CLI's public surface, see `.claude/rules/cli-skill-sync.md` for which skill file to update alongside it.
