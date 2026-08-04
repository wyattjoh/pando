# AGENTS.md

Guidance for coding agents working in this repository. `CLAUDE.md` is a symlink to this file.

## Commands

```sh
just build            # cargo build --all-features
just lint             # cargo fmt --check, then Clippy with CI settings
just test             # cargo test --all-features
just install-hooks    # prek install
just install          # cargo install --path .

cargo test --test cli switch_creates          # one integration test (substring match)
cargo test --lib                              # unit tests only (in-module #[cfg(test)])
cargo test --test cli -- --nocapture          # see PTY/child output
```

CI runs `fmt --check`, `clippy -D warnings`, and `test --all-features` on ubuntu-latest and macos-latest. Clippy `pedantic` is `warn` in `Cargo.toml` but denied in CI, so public fallible functions need `/// # Errors` doc sections and public types need `#[must_use]` where applicable.

Edition 2024, MSRV 1.85. Unix-only: the code uses `std::os::unix` APIs directly (`OsStrExt`, `ExitStatusExt`, `/bin/sh`, `/dev/stderr`).

## Architecture

`pando` is a CLI for inspecting, creating, and navigating the Git worktrees of the repository containing the current directory. **Git is the source of truth**: every repository fact comes from invoking the installed `git` executable (no libgit2, no cached registry, no implicit fetch).

`src/main.rs` is a thin clap dispatcher over `src/lib.rs`; all logic lives in the library so integration tests and future callers can reach it.

### Module map

| Module | Responsibility |
|---|---|
| `lib.rs` | `Worktree`/`WorktreeKind`/`Condition`/`SortMode` domain types, display/navigability rules, and stable worktree sorting |
| `git.rs` | Private Git execution and parsing internals plus concrete repository observation, worktree mutation, history observation, and lifecycle mutation interfaces |
| `branch.rs` | Concrete branch/ref resolution interface; classification, targets, bases, fetch applicability, upstreams, remotes, and push planning |
| `config.rs` | Strict three-layer YAML config parsing and effective value resolution |
| `pr.rs`, `pr/provider.rs` | PR orchestration and metadata generation; pluggable `gh` and `tea` forge adapters |
| `smart.rs` | Command implementations for `switch`/`create`/`get`/`trust`; all interactive prompts |
| `squash.rs` | Merge-time branch collapse: squash planning, prompt rendering, generator subprocess, `reset --soft` plus commit |
| `trust.rs` | Post-create hook approval: command hashing, XDG `trust.json`, atomic writes |
| `setup.rs` | Post-create hook execution and the incomplete-setup journal |
| `install.rs` | Managed zsh integration, marker-block rewriting, connected-agent detection, and LLM-guided global configuration |
| `completion.rs` | Best-effort candidate producers for dynamic zsh completion of branch arguments |
| `render.rs` | Column alignment shared by `list` output and the picker's menu labels, plus the shared styling for captured Git output and commit messages |
| `hash.rs` | Hex encoding shared by `trust.rs` and `setup.rs` |

Git ownership is capability-based. `RepositoryObservation` owns discovery and repository facts; `branch::Resolver` and `BranchRepository` own ref planning; `WorktreeMutation` owns worktree changes; `HistoryObservation` owns history and diffs; `LifecycleMutation` owns index and history-changing lifecycle operations; private parsers own structured output; and private `GitProcess` owns execution policy. These interfaces return typed semantic results, not command-shaped forwarding APIs or raw subprocess policy. Do not add direct Git process construction outside `src/git.rs`, a public Git trait, fake adapter, cache, registry, or alternate repository implementation. `.claude/rules/git-ownership.md` contains the detailed routing guidance, and `tests/git_architecture.rs` enforces the process and execution-kernel boundary.

### Load-bearing invariants

**stdout purity.** The binary writes *only* a successful destination path (`switch`) or a single property value (`get`) to stdout, always with a trailing newline. Prompts, warnings, hook output, and guided installer agent output go to stderr. The installed zsh function captures stdout and `cd`s to it, so any stray `println!` in a `switch` path silently breaks directory switching. Note the asymmetry: `finish_setup` writes the destination *before* returning an error on hook failure, so the shell still enters a half-configured worktree while preserving the nonzero status.

**Paths are bytes, not strings.** Destinations are written with `write_all(path.as_os_str().as_bytes())` rather than `Display`, and porcelain parsing goes through `OsString::from_vec`. Non-UTF-8 and space-containing paths are covered by tests — don't introduce `to_string_lossy` on a path that reaches stdout.

**Config layering** (`EffectiveConfig::load`) is deliberately asymmetric:

- global `$XDG_CONFIG_HOME/pando/config.yaml` controls placement and the personal default sort, never hooks;
- `.pando.yaml` read from the **invoking** worktree controls shared hooks, the target branch, and the new-branch base, never placement or personal sort, so setup follows the branch you create from;
- `.pando.local.yaml` read from the **primary** worktree controls personal placement, hooks, default sort, and the new-branch base, and must be Git-ignored or loading is a hard error.

`worktrees.base`, `pr.provider`, `merge.squash`, and `merge.generation` are legal in all three layers, resolving local over shared over global. `install.command` is global-only and lives in a marker block managed by guided `pando install`; it records the editable connected-agent command, not a generator. `worktrees.base` defaults to `head`; `head` starts a genuinely new branch at the invoking worktree's `HEAD`, while `fresh` starts it at the remote-tracking ref of the configured `target-branch`, or of the branch named by the remote's `origin/HEAD`. `fresh` reads local refs only. An unresolvable or never-fetched base is a hard error naming the fix, and `--fetch` on `switch`/`create` is the only way to refresh it, fetching exactly that one ref. `pr.provider` defaults to `auto`, selecting `gh` for github.com and `tea` for other forge hosts with a matching configured tea login.

Shared hooks run before local hooks; the local root and default sort override their global values. Human list and picker rendering use the effective sort, while structured JSON always retains Git discovery order. All structs use `deny_unknown_fields`, so config mistakes fail loudly with file context.

**PR forge behavior lives behind one seam.** `pr.rs` owns provider-neutral repository checks, metadata, publication, and rendering. `pr/provider.rs` owns remote parsing and every `gh` or `tea` subprocess. Auto-detection uses `gh` only for github.com; every other host must match a configured tea login. Both adapters use the same base/head repository model and open-PR/create interface. Tea drafts use the conventional `WIP:` title prefix for compatibility across Tea versions. Tea creation output is not stable across versions: v0.15 wraps the URL in an OSC-8 hyperlink when stdout is captured. The adapter extracts URLs from plain or control-wrapped output and falls back to a post-create base/head lookup when none is present.

**Merge squashes by default, between the rebase and the fast-forward.** `squash.rs` owns the collapse; `lifecycle.rs` owns when it happens. The ordering is load-bearing: after the rebase the target is already an ancestor of the topic, so the squash is a `reset --soft <target>` plus one commit and needs no merge base. Squashing before validation means the pre-merge hooks and `validated_source` see the commit that actually lands. A topic that is already one commit is skipped entirely rather than having its message rewritten, which is also why `merge.squash` needs no special case for repeated runs. The journal pins `no_squash` with the other policy flags and records `squashed`, so a retry after a later failure cannot collapse twice. `merge.generation.command` falls back to `commit.generation.command`, but trust does not: the two have separate domains (`pando-merge-generation-v1`) and separate `trust.json` namespaces, so approving one never approves the other. A multi-commit topic with no resolvable generator is a preflight error, not a silent unsquashed merge. Generation and the collapse are separate steps so the rail can print the message before history is rewritten; `render::commit_message` is shared with `commit` so a generated message looks the same wherever it appears, and `squash::collapse` drops Git's transcript because the message and the following fast-forward already report it.

**Trust identity is command strings only.** `trust::command_hash` digests a domain-separated, length-prefixed concatenation of the ordered `command` fields — names, comments, and formatting are excluded on purpose, so reordering or editing a command revokes approval but renaming a step does not. Approval is keyed by the hex-encoded canonical primary-worktree path and there is no non-interactive bypass; `ensure_interactive` refuses rather than assuming yes.

**The incomplete-setup journal is two-phase** because a worktree's stable identity doesn't exist until Git creates it. `setup::prepare` writes a *pending* record keyed by branch hash **before** the `git worktree add`; after creation, `PendingRecord::commit` renames it to a *marker* keyed by the hash of the new worktree's git dir. Both live under `<common-dir>/pando-state/`. `is_incomplete`/`clear` check both locations, which is why `branch` is threaded through as an `Option`. Creation failure calls `cancel()`. Writes go through `trust::write_atomic` (temp file + `sync_all` + `rename`).

**Port hashing is pinned for compatibility.** `smart::port_for_branch` uses `SipHasher13` explicitly rather than `DefaultHasher` so a future std change cannot move ports away from Worktrunk v0.66.0. Golden values are asserted in `smart.rs` tests — treat them as a compatibility contract.

**Branch resolution order** in `resolve_and_switch`: existing registered worktree → existing local branch → single already-fetched remote match → prompt among multiple remotes → confirm a genuinely new branch from the base `worktrees.base` selects. `branch::Resolver` owns that order and never prompts or implicitly selects an ambiguous remote, so dry runs and adapters share the same typed resolution; only the new-branch arm's start point is configurable. The tool never adopts, repairs, prunes, moves, or deletes an existing destination or a broken worktree record.

**One planner owns the new-branch start point.** `git::plan_new_branch_base` is the single place any interface resolves it, so human `switch`/`create`, their dry runs, and both JSON variants cannot drift. Dry runs call it with fetching disabled and report the refresh as an unattempted effect instead.

**Worktree mutation consumes semantic plans.** `git::WorktreeMutation` is the only interface for creating, describing, or removing worktrees. Callers select `WorktreeSource`, `RemovalMode`, and `RemovalOutput`; they never assemble `git worktree` arguments or expose generic stream booleans. Git remains the destination-safety authority, descriptions stay after creation callbacks, and removal always retains the branch.

**Machine mode is only a protocol adapter.** `machine.rs` parses strict requests, routes them to command-owned typed interfaces, adapts outcomes through `protocol.rs`, writes one response, and selects the exit status. Planning, mutation, effect construction, diagnostics, recovery actions, stable error catalogs, and stable action catalogs belong to command modules. Do not reconstruct command state in the adapter or capture human output to produce JSON.

**JSON `merge` invokes the lifecycle executor directly.** `lifecycle::execute_merge_request` owns planning and execution for the typed request, including phase effects, diagnostics, journal context, and recovery. Human merge rendering and `machine::merge` consume that command-owned result without replanning or inferring completed phases.

**`switch` and `create` share one operation, parameterized by `Intent`.** `worktree_plan::operation` owns classification, planning, execution, effects, and recovery for both adapters. `Intent::Create` skips only the genuinely-new-branch confirmation and refuses an already-registered branch; `create` remains the sole JSON entry point allowed to create a branch unattended, while `switch` returns `switch.approval_required`. Keep intent checks narrow so human and JSON paths cannot drift.

### Testing

`tests/cli.rs` is the bulk of the suite and is behavioral end-to-end: it builds real temporary Git repositories, runs the compiled binary via `assert_cmd`, and drives interactive flows through a real PTY (`nix::pty::openpty`) so `IsTerminal` checks behave as they do in a shell. Tests isolate `HOME`/`XDG_CONFIG_HOME`/`ZDOTDIR` into temp dirs, and the zsh integration tests spawn actual `zsh`. Unit tests stay in-module for pure logic (porcelain parsing, `.zshrc` block rewriting, port goldens).

When adding a feature, prefer an integration test that asserts the stdout/stderr split and exit status, not just the happy path.

## Agent skills

### Issue tracker

Issues and specs are tracked as local markdown under `.scratch/<feature>/`. See `docs/agents/issue-tracker.md`.

### Domain docs

This repository uses a single-context domain-doc layout. See `docs/agents/domain.md`.

### CLI usage skill

`skills/pando/` documents `pando`' own command surface, flags, config schema, and JSON contract for kickstarting usage (symlinked into `.claude/skills/pando` and `.agents/skills/pando`). Whenever a change touches the CLI's public surface, see `.claude/rules/cli-skill-sync.md` for which skill file to update alongside it.
