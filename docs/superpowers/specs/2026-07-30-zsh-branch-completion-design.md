# Zsh branch completion

Design for tab-completing branch arguments (and the rest of the CLI surface) in
zsh for `worktrees` and its `wt` alias.

## Problem

`wt switch <branch>`, `wt create <branch>`, and `wt remove <branches>...` take
branch names that the user must type in full. Nothing completes them, nor the
subcommands, flags, or value enums that clap already knows about.

## Approach

Use `clap_complete`'s dynamic completion (`CompleteEnv`). zsh calls back into the
binary on every Tab, so branch candidates come from Rust and the static surface
(subcommands, flags, `--output` and `get` value enums) is derived from the clap
`Command` and cannot drift from `src/main.rs`.

The rejected alternative was a hand-written `_worktrees` zsh function embedded in
the install blob. It needs no new dependency, but duplicates the entire flag
surface in shell and drifts silently against `main.rs`.

## Dependencies

```toml
clap = { version = "4.5", features = ["derive", "unstable-ext"] }
clap_complete = { version = "4.6", features = ["unstable-dynamic"] }
```

`unstable-ext` is required by the `#[arg(add = ...)]` attribute; `unstable-dynamic`
gates `CompleteEnv`, `ArgValueCandidates`, and `CompletionCandidate`. Both are
explicitly unstable clap APIs. They are pinned to `4.6` and covered by tests, so a
breaking change surfaces in CI rather than silently degrading completion.
`clap_complete` 4.6.8 declares MSRV 1.85, matching this crate.

## Entry point

`src/main.rs`, as the first statement of `main()`:

```rust
clap_complete::CompleteEnv::with_factory(Cli::command).complete();
```

`complete()` is a no-op unless `COMPLETE` is set in the environment; when it is
set, it prints and exits the process. It must precede the existing `args_os`
JSON sniffing so a completion invocation never enters the protocol path.

## Candidate producers

A new module `src/completion.rs` owns the three producers, keeping `main.rs` a
thin dispatcher. Each returns `Vec<CompletionCandidate>`.

| Producer | Offers |
|---|---|
| `switch_candidates` | local branches; plus refs under `refs/remotes` whose short name has no local branch, each helped `remote branch` |
| `create_candidates` | the same set, minus branches already registered as a worktree |
| `remove_candidates` | only branches with a registered non-primary worktree; candidate help text is the worktree path |

Remote candidates are distinguished by help text (`remote branch`), not by a
group heading. `clap_complete`'s stock zsh script funnels every candidate through
a single `_describe -V 'values'` call and ignores `CompletionCandidate::tag`, so
per-group headings are not reachable without replacing that script. Help text is
rendered beside each value, which achieves the distinction the grouping was for.

Producers return the full candidate set and do not filter by the partial word.
`clap_complete`'s engine applies prefix filtering itself.

Every producer is best-effort. A git failure, a cwd outside a repository, or a
malformed ref yields an empty `Vec` — never an `Err`, never output on stderr. A
completion widget that prints a diagnostic corrupts the user's command line.

Wiring, on each of the three branch arguments in `src/main.rs`:

```rust
Switch {
    #[arg(add = ArgValueCandidates::new(completion::switch_candidates))]
    branch: Option<String>,
    // ...
}
```

`src/git.rs` gains one helper, `discover_remote_branches`, a `for-each-ref
refs/remotes` wrapper following the existing helpers' shape (byte-oriented
parsing, no `to_string_lossy` on anything path-like). Local branches and worktree
registration come from the existing `discover_branches` and `repository`.

## Install wiring

`INTEGRATION` in `src/install.rs` stays a `&[u8]` const and gains a trailing
block that evaluates the registration script at shell startup:

```zsh
if [[ -o interactive ]] && (( $+functions[compdef] )); then
  eval "$(COMPLETE=zsh command worktrees 2>/dev/null)"
  compdef _clap_dynamic_completer_worktrees wt
fi
```

The registration script is **generated at startup, not cached**. `clap_complete`
documents no stability guarantee between the script `write_registration` emits
and the protocol `write_complete` expects, and explicitly warns that caching the
script "may result in invalid or no completions". Caching it in `worktrees.zsh`
would silently break completion whenever the binary is upgraded without re-running
`wt install`. Generating it costs one process spawn per interactive shell,
measured at under 3ms in a debug build.

This also keeps `install.rs` unchanged in shape: `INTEGRATION` remains a const, so
`install::run`, `install::preview`, `install::json_plan`, and `machine::install`
keep their current signatures, and `Cli` stays in `main.rs`.

`wt` is a symlink to the `worktrees` binary (see `justfile`), so one registration
plus one extra `compdef` line covers both names. `CompleteEnv::completer` defaults
to `args_os()[0]`, so invoking the eval as `command worktrees` bakes the bare name
`worktrees` into the script rather than an absolute path, keeping it valid if the
binary moves.

The `compdef` guard matters: if the user's `.zshrc` runs `compinit` after the
worktrees block, `compdef` does not yet exist. The block then skips registration
rather than erroring, and a `precmd` one-shot retry re-attempts it once startup
finishes.

Everything else about installation is unchanged: one file, one atomic write, one
`integration_changed` comparison, and the existing marker-block idempotency.
Existing users pick up completion on their next `wt install`.

The existing `install_preserves_zshrc_and_is_idempotent` test asserts no line in
`worktrees.zsh` begins with `_`, because function-table snapshots drop `_name`
functions. That assertion still holds: `_clap_dynamic_completer_worktrees` is only
referenced mid-line in a `compdef` argument, and is defined at runtime by the
eval'd script, where the underscore prefix is the completion system's own
convention.

## Invariants preserved

**stdout purity.** Completion output reaches stdout only when `COMPLETE` is set.
In that invocation the shell function's `$1` is `--`, not `switch`/`create`/
`remove`/`merge`, so `worktrees_dispatch` takes its passthrough branch and never
attempts a `cd`.

**Paths are bytes.** `discover_remote_branches` parses `for-each-ref` output as
bytes. Branch names that are not valid UTF-8 are dropped from the candidate list
rather than lossily converted, because `CompletionCandidate` is string-typed.

## Testing

New cases in `tests/cli.rs`. None require a PTY — completion is driven entirely
by the `COMPLETE` environment variable.

- `COMPLETE=zsh` with no arguments emits a registration script containing
  `compdef`.
- In a temp repo with `main`, `feat-a` (has a worktree), and `feat-b` (no
  worktree): `switch ''` offers all three; `create ''` excludes `feat-a`;
  `remove ''` offers only `feat-a`.
- Prefix filtering: `switch fea` offers only the `feat-*` branches.
- A cwd outside any repository exits 0 with empty stdout and empty stderr.
- The existing zsh integration test is extended: after `install`, the generated
  `worktrees.zsh` sources cleanly under real zsh and `compdef` is registered for
  both `worktrees` and `wt`.

## Documentation

Per `.claude/rules/cli-skill-sync.md`, the install surface changes, so update
`skills/worktrees/references/commands/install.md` and
`skills/worktrees/SKILL.md`. Also update the README's install section and add
`completion.rs` to the module map in `CLAUDE.md`.

## Out of scope

- **`remove` re-offers already-typed branches.** clap's candidate API receives no
  prior-word context, so deduplicating against the current command line would
  require hand-writing zsh — the approach this design rejects.
- **Only zsh is registered.** `CompleteEnv` supports bash and fish, but
  `worktrees install` manages zsh alone today. Extending it is a separate change.
