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
| `switch_candidates` | local branches, grouped `local branches`; plus refs under `refs/remotes` whose short name has no local branch, grouped `remote branches` |
| `create_candidates` | the same set, minus branches already registered as a worktree |
| `remove_candidates` | only branches with a registered non-primary worktree; candidate help text is the worktree path |

Grouping uses `CompletionCandidate::tag`, which zsh renders as a heading above
each group.

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

`INTEGRATION` in `src/install.rs` changes from a `&[u8]` const to a computed
`Vec<u8>`: the existing const, followed by `clap_complete`'s generated zsh
registration script, followed by `compdef _clap_dynamic_completer_worktrees wt`.

`wt` is a symlink to the `worktrees` binary (see `justfile`), so one registration
plus one extra `compdef` line covers both names.

The registration script is generated from `Cli::command()`, which lives in
`main.rs` and is not visible to the library. Rather than move `Cli` into the
library, the four install entry points — `install::run`, `install::preview`,
`install::json_plan`, and `machine::install` — take the registration bytes as a
parameter. This keeps the change local at the cost of one extra argument
threaded through those call sites.

Everything else about installation is unchanged: one file, one atomic write, one
`integration_changed` comparison, and the existing marker-block idempotency.
Existing users pick up completion on their next `wt install`.

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
