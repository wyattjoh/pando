# Zsh Branch Completion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Tab-complete branch arguments for `pando switch|create|remove` in zsh, plus every subcommand, flag, and value enum, for both the `pando` and `pd` names.

**Architecture:** `clap_complete`'s dynamic completion. zsh calls back into the binary on each Tab via a `COMPLETE=zsh` environment protocol. Branch candidates are produced in Rust from `git.rs`; the static surface derives from the clap `Command` and cannot drift from `main.rs`. The managed `pando.zsh` that `pando install` writes gains a block that evaluates the registration script at shell startup.

**Tech Stack:** Rust (edition 2024, MSRV 1.85), clap 4 derive, `clap_complete` 4.6 `unstable-dynamic`, zsh, `assert_cmd` + `tempfile` integration tests.

## Global Constraints

- Edition 2024, MSRV 1.85. Unix-only.
- CI denies warnings: `cargo clippy --all-targets --all-features -- -D warnings`. Clippy `pedantic` is `warn` in `Cargo.toml` but denied in CI, so **every new public fallible function needs a `/// # Errors` section** and public types need `#[must_use]` where applicable.
- `cargo fmt --check` must pass.
- **stdout purity:** the binary writes only a destination path (`switch`/`create`) or a single property value (`get`) to stdout. Completion output is the one new exception and only occurs when `COMPLETE` is set in the environment.
- **Paths are bytes:** never introduce `to_string_lossy` on a path that reaches stdout. Git ref output is parsed as bytes.
- Never use em dashes in comments, commit messages, or docs. Use commas, parentheses, or separate sentences.
- Commits use Conventional Commits: `<type>[optional scope]: <description>`.
- Completion producers are **best-effort and infallible**: they return `Vec<CompletionCandidate>`, never `Result`. Any git failure yields an empty `Vec`. They must never write to stdout or stderr.
- `clap_complete`'s registration script must **never be cached to disk**. The crate documents no stability guarantee between `write_registration` and `write_complete`.

## Reference

Spec: `docs/superpowers/specs/2026-07-30-zsh-branch-completion-design.md`

The completion protocol, verified against `clap_complete` 4.6.8:

```
# Registration script to stdout, exit 0:
COMPLETE=zsh pando

# Candidates to stdout as `value:help` or `value`, newline separated, exit 0:
_CLAP_COMPLETE_INDEX=<n> COMPLETE=zsh pando -- <word0> <word1> ... <wordN>
```

`_CLAP_COMPLETE_INDEX` is the 0-based index of the word being completed. Prefix
filtering is applied by the engine, so producers return the full set.

## File Structure

| File | Responsibility |
|---|---|
| `Cargo.toml` | Add `clap_complete`; add `unstable-ext` to `clap` |
| `src/completion.rs` (create) | The three candidate producers. Sole owner of completion policy. |
| `src/git.rs` (modify) | Add `discover_remote_branches`, a `for-each-ref refs/remotes` wrapper |
| `src/lib.rs` (modify) | `pub mod completion;` |
| `src/main.rs` (modify) | `CompleteEnv::complete()` as first statement of `main()`; `#[arg(add = ...)]` on three branch args |
| `src/install.rs` (modify) | Append the compdef registration block to `INTEGRATION` |
| `tests/cli.rs` (modify) | Completion integration tests |
| `skills/pando/`, `README.md`, `CLAUDE.md` | Docs |

---

### Task 1: Add dependencies and the completion entry point

Wires `CompleteEnv` into `main()` so the static surface (subcommands, flags,
`--output` and `get` value enums) completes. No branch candidates yet.

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/main.rs`
- Test: `tests/cli.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: the `COMPLETE=zsh` protocol on the `pando` binary. Later tasks
  attach candidates to arguments; nothing else calls into this.

- [ ] **Step 1: Write the failing tests**

Append to `tests/cli.rs`:

```rust
#[test]
fn complete_env_emits_a_zsh_registration_script() {
    let mut command = Command::cargo_bin("pando").unwrap();
    let output = command.env("COMPLETE", "zsh").output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("#compdef pando"));
    assert!(stdout.contains("compdef _clap_dynamic_completer_pando pando"));
}

#[test]
fn complete_env_completes_subcommands() {
    let candidates = complete(&std::env::current_dir().unwrap(), &["pando", ""]);

    assert!(candidates.iter().any(|value| value == "switch"));
    assert!(candidates.iter().any(|value| value == "create"));
    assert!(candidates.iter().any(|value| value == "remove"));
}

#[test]
fn complete_env_completes_flags() {
    let candidates = complete(&std::env::current_dir().unwrap(), &["pando", "--out"]);

    assert!(candidates.iter().any(|value| value == "--output"));
}

#[test]
fn complete_env_completes_get_properties() {
    let candidates = complete(&std::env::current_dir().unwrap(), &["pando", "get", ""]);

    assert!(candidates.iter().any(|value| value == "branch"));
    assert!(candidates.iter().any(|value| value == "port"));
    assert!(candidates.iter().any(|value| value == "worktree-path"));
}

/// Drives the `COMPLETE=zsh` protocol and returns candidate values with any
/// `:help` suffix stripped. The final entry of `words` is the word being
/// completed, matching how zsh passes `${words[@]}`.
fn complete(dir: &Path, words: &[&str]) -> Vec<String> {
    let index = words.len() - 1;
    let mut command = Command::cargo_bin("pando").unwrap();
    let output = command
        .env("COMPLETE", "zsh")
        .env("_CLAP_COMPLETE_INDEX", index.to_string())
        .arg("--")
        .args(words)
        .current_dir(dir)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "completion failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| line.split_once(':').map_or(line, |(value, _)| value).to_owned())
        .collect()
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test cli complete_env`
Expected: FAIL. `complete_env_emits_a_zsh_registration_script` fails because with
`COMPLETE=zsh` and no arguments the binary currently prints a clap "missing
subcommand" error to stderr and exits 2.

- [ ] **Step 3: Add the dependencies**

In `Cargo.toml`, change the `clap` line and add `clap_complete` directly beneath it:

```toml
clap = { version = "4.5", features = ["derive", "unstable-ext"] }
clap_complete = { version = "4.6", features = ["unstable-dynamic"] }
```

`unstable-ext` is required by the `#[arg(add = ...)]` attribute used in Task 3.
`unstable-dynamic` gates `CompleteEnv` and `ArgValueCandidates`.

- [ ] **Step 4: Wire the entry point**

In `src/main.rs`, add `CommandFactory` to the clap import:

```rust
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
```

Then make this the **first statement** of `fn main()`, above the existing
`let args: Vec<_> = env::args_os().collect();`:

```rust
fn main() {
    // Must precede every other statement: `complete` writes candidates to stdout
    // and exits when `COMPLETE` is set, and clap documents that stdout must not
    // be written to beforehand. Placing it here also keeps a completion request
    // out of the `--output json` protocol path below.
    clap_complete::CompleteEnv::with_factory(Cli::command).complete();

    let args: Vec<_> = env::args_os().collect();
    // ... existing body unchanged
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --test cli complete_env`
Expected: PASS, all four.

- [ ] **Step 6: Verify the full suite and lints still pass**

Run: `cargo fmt && cargo clippy --all-targets --all-features -- -D warnings && cargo test --all-features`
Expected: clean. In particular confirm no existing test regressed: `complete()`
returns `Ok(false)` when `COMPLETE` is unset, so normal invocations are untouched.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock src/main.rs tests/cli.rs
git commit -m "feat(completion): complete subcommands and flags in zsh"
```

---

### Task 2: Add remote branch discovery to git.rs

`git.rs` can already list local branches (`discover_branches`) and registered
worktrees (`repository`). It cannot list remote-tracking refs, which
`switch_candidates` needs.

**Files:**
- Modify: `src/git.rs`
- Test: `src/git.rs` (in-module `#[cfg(test)]`, following the existing
  `parse_porcelain` and `parse_branch_refs` unit tests)

**Interfaces:**
- Consumes: the existing private `run_git`, `ensure_success` helpers in `git.rs`.
- Produces:
  - `pub fn discover_remote_branches(cwd: &Path) -> Result<Vec<String>>` returning
    short names such as `origin/feature`, excluding any symbolic ref such as
    `origin/HEAD`, in Git's own sort order.
  - `fn parse_remote_branch_refs(bytes: &[u8]) -> Vec<String>` (private, unit tested).

- [ ] **Step 1: Write the failing unit test**

Add to the existing `#[cfg(test)] mod tests` block at the bottom of `src/git.rs`.
Add `parse_remote_branch_refs` to that module's existing `use super::{...}` import list.

```rust
#[test]
fn remote_branch_refs_are_parsed_and_symbolic_refs_excluded() {
    // `<refname:short>\0<symref>\0` per record. `origin/HEAD` carries a symref
    // target and must be excluded: it is a pointer, not a branch.
    let bytes = b"origin/main\x00\x00origin/HEAD\x00refs/remotes/origin/main\x00upstream/dev\x00\x00";

    assert_eq!(
        parse_remote_branch_refs(bytes),
        vec!["origin/main".to_owned(), "upstream/dev".to_owned()]
    );
}

#[test]
fn non_utf8_remote_branch_refs_are_dropped() {
    let bytes = b"origin/good\x00\x00origin/ba\xffd\x00\x00";

    assert_eq!(
        parse_remote_branch_refs(bytes),
        vec!["origin/good".to_owned()]
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib remote_branch_refs`
Expected: FAIL with "cannot find function `parse_remote_branch_refs`".

- [ ] **Step 3: Implement**

Add to `src/git.rs`, directly beneath the existing `discover_branches` function:

```rust
/// Lists remote-tracking branches as short names such as `origin/feature`.
///
/// Only `refs/remotes` is inspected; no fetch happens. Symbolic refs such as
/// `origin/HEAD` are excluded because they point at a branch rather than being
/// one. A ref whose short name is not valid UTF-8 is dropped.
///
/// # Errors
///
/// Returns an error when Git cannot run or fails to list remote refs.
pub fn discover_remote_branches(cwd: &Path) -> Result<Vec<String>> {
    let output = run_git(
        cwd,
        [
            "for-each-ref",
            "--format=%(refname:short)%00%(symref)%00",
            "refs/remotes",
        ],
    )
    .context("failed to list remote branches")?;
    ensure_success(&output, "git for-each-ref")?;
    Ok(parse_remote_branch_refs(&output.stdout))
}

/// Parses NUL-delimited `<refname:short>\0<symref>\0` records.
fn parse_remote_branch_refs(bytes: &[u8]) -> Vec<String> {
    let mut records = Vec::new();
    let mut fields = bytes.split(|byte| *byte == 0);
    loop {
        let Some(raw_name) = fields.next() else {
            break;
        };
        let name_field = raw_name.strip_prefix(b"\n").unwrap_or(raw_name);
        if name_field.is_empty() {
            break;
        }
        let Some(raw_symref) = fields.next() else {
            break;
        };
        let symref_field = raw_symref.strip_prefix(b"\n").unwrap_or(raw_symref);
        if !symref_field.is_empty() {
            continue;
        }
        if let Ok(name) = std::str::from_utf8(name_field) {
            records.push(name.to_owned());
        }
    }
    records
}
```

The `strip_prefix(b"\n")` handling mirrors `parse_branch_refs` above it: Git
separates records with a newline that lands at the front of the next field.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib remote_branch_refs`
Expected: PASS, both.

- [ ] **Step 5: Verify lints**

Run: `cargo fmt && cargo clippy --all-targets --all-features -- -D warnings`
Expected: clean. `discover_remote_branches` is public and fallible, so the
`/// # Errors` section above is mandatory.

- [ ] **Step 6: Commit**

```bash
git add src/git.rs
git commit -m "feat(git): list remote-tracking branches"
```

---

### Task 3: Branch candidates for switch, create, and remove

**Files:**
- Create: `src/completion.rs`
- Modify: `src/lib.rs`
- Modify: `src/main.rs`
- Test: `tests/cli.rs`

**Interfaces:**
- Consumes (all verified against the current source):
  - `git::discover_branches(&Path) -> Result<Vec<BranchRecord>>`; `BranchRecord`
    has a `pub branch: String`.
  - `git::discover_remote_branches(&Path) -> Result<Vec<String>>` from Task 2.
  - `git::repository(&Path) -> Result<Repository>`; `Repository` has
    `pub worktrees: Vec<Worktree>` and `pub primary: Option<PathBuf>`.
  - `Worktree` has `pub path: PathBuf` and `pub kind: WorktreeKind`. There is
    **no** `branch` field and **no** `is_primary()` method. A worktree's branch is
    `WorktreeKind::Branch(String)`; the other variants are `Detached`, `Bare`, and
    `Unknown`. Primary is `Some(&worktree.path) == repository.primary.as_ref()`,
    which is exactly how `lifecycle.rs` rejects removing the primary worktree.
- Produces:
  - `pub fn switch_candidates() -> Vec<CompletionCandidate>`
  - `pub fn create_candidates() -> Vec<CompletionCandidate>`
  - `pub fn remove_candidates() -> Vec<CompletionCandidate>`

  All three are zero-argument so they can be passed directly to
  `ArgValueCandidates::new`.

`remove_candidates` must mirror `lifecycle.rs`'s target resolution exactly: a
valid target is a worktree whose `kind` is `Branch(name)` and whose path is not
the primary. Anything else is rejected at runtime, so offering it would be a lie.

- [ ] **Step 1: Write the failing tests**

Append to `tests/cli.rs`. These reuse the `complete()` helper from Task 1 and the
existing `Repository` fixture, whose `new()` creates branch `main` in
`repo.main` and branch `feature` in a linked worktree.

```rust
#[test]
fn switch_completes_local_and_remote_branches() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "solo"]);
    // A remote-tracking ref without a matching local branch. Created directly
    // so the test needs no network and no second repository.
    git(
        &repo.main,
        ["update-ref", "refs/remotes/origin/remote-only", "HEAD"],
    );

    let candidates = complete(&repo.main, &["pando", "switch", ""]);

    assert!(candidates.iter().any(|value| value == "main"));
    assert!(candidates.iter().any(|value| value == "feature"));
    assert!(candidates.iter().any(|value| value == "solo"));
    assert!(candidates.iter().any(|value| value == "origin/remote-only"));
}

#[test]
fn switch_hides_remote_refs_that_shadow_a_local_branch() {
    let repo = Repository::new();
    git(&repo.main, ["update-ref", "refs/remotes/origin/main", "HEAD"]);

    let candidates = complete(&repo.main, &["pando", "switch", ""]);

    assert!(candidates.iter().any(|value| value == "main"));
    assert!(
        !candidates.iter().any(|value| value == "origin/main"),
        "a remote ref shadowing a local branch is redundant: {candidates:?}"
    );
}

#[test]
fn create_excludes_branches_that_already_have_a_worktree() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "solo"]);

    let candidates = complete(&repo.main, &["pando", "create", ""]);

    assert!(candidates.iter().any(|value| value == "solo"));
    assert!(
        !candidates.iter().any(|value| value == "feature"),
        "feature already has a worktree: {candidates:?}"
    );
    assert!(
        !candidates.iter().any(|value| value == "main"),
        "main is the primary worktree: {candidates:?}"
    );
}

#[test]
fn remove_offers_only_branches_with_a_topic_worktree() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "solo"]);

    let candidates = complete(&repo.main, &["pando", "remove", ""]);

    assert!(candidates.iter().any(|value| value == "feature"));
    assert!(
        !candidates.iter().any(|value| value == "solo"),
        "solo has no worktree to remove: {candidates:?}"
    );
    assert!(
        !candidates.iter().any(|value| value == "main"),
        "the primary worktree is not removable: {candidates:?}"
    );
}

#[test]
fn branch_completion_filters_by_prefix() {
    let repo = Repository::new();
    git(&repo.main, ["branch", "feat-a"]);
    git(&repo.main, ["branch", "other"]);

    let candidates = complete(&repo.main, &["pando", "switch", "feat"]);

    assert!(candidates.iter().any(|value| value == "feat-a"));
    assert!(!candidates.iter().any(|value| value == "other"));
}

#[test]
fn branch_completion_outside_a_repository_is_silent() {
    let outside = tempfile::tempdir().unwrap();

    let mut command = Command::cargo_bin("pando").unwrap();
    let output = command
        .env("COMPLETE", "zsh")
        .env("_CLAP_COMPLETE_INDEX", "2")
        .arg("--")
        .args(["pando", "switch", ""])
        .current_dir(outside.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(
        output.stderr.is_empty(),
        "a completion widget must never print diagnostics: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        !stdout.lines().any(|line| line.contains("error")),
        "no error text may reach the completion line: {stdout}"
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test cli -- switch_completes switch_hides create_excludes remove_offers branch_completion`
Expected: FAIL. No branch candidates are produced yet, so each assertion for a
branch name fails.

- [ ] **Step 3: Create the completion module**

Create `src/completion.rs`:

```rust
//! Candidate producers for dynamic shell completion.
//!
//! Every producer is best-effort and infallible. A completion widget owns the
//! user's command line, so a git failure, a cwd outside a repository, or a
//! malformed ref must yield an empty list rather than an error or any output on
//! stderr.

use std::{
    collections::HashSet,
    env,
    path::{Path, PathBuf},
};

use clap_complete::CompletionCandidate;

use crate::{WorktreeKind, git};

/// Branches `switch` accepts: every local branch, plus remote-tracking refs that
/// no local branch already shadows.
#[must_use]
pub fn switch_candidates() -> Vec<CompletionCandidate> {
    let Some(cwd) = cwd() else {
        return Vec::new();
    };
    let local = local_branches(&cwd);
    let mut candidates: Vec<_> = local.iter().map(CompletionCandidate::new).collect();
    candidates.extend(remote_candidates(&cwd, &local));
    candidates
}

/// Branches `create` accepts: those without a registered worktree. `Intent::Create`
/// refuses a branch that already has one.
#[must_use]
pub fn create_candidates() -> Vec<CompletionCandidate> {
    let Some(cwd) = cwd() else {
        return Vec::new();
    };
    let registered = registered_branches(&cwd);
    let local = local_branches(&cwd);
    let mut candidates: Vec<_> = local
        .iter()
        .filter(|branch| !registered.contains(*branch))
        .map(CompletionCandidate::new)
        .collect();
    // Remote refs need no separate exclusion: a registered branch is always a
    // local branch, so `remote_candidates` has already dropped any remote ref
    // shadowed by one. Note it is passed the unfiltered local list.
    candidates.extend(remote_candidates(&cwd, &local));
    candidates
}

/// Branches `remove` accepts: those with a registered non-primary worktree. The
/// candidate's help text is the worktree path, matching what `list` shows.
///
/// This mirrors `lifecycle::resolve_targets`, which finds the worktree whose kind
/// is `Branch(name)` and rejects the primary.
#[must_use]
pub fn remove_candidates() -> Vec<CompletionCandidate> {
    let Some(cwd) = cwd() else {
        return Vec::new();
    };
    let Ok(repository) = git::repository(&cwd) else {
        return Vec::new();
    };
    repository
        .worktrees
        .iter()
        .filter(|worktree| Some(&worktree.path) != repository.primary.as_ref())
        .filter_map(|worktree| {
            let WorktreeKind::Branch(branch) = &worktree.kind else {
                return None;
            };
            Some(
                CompletionCandidate::new(branch)
                    .help(Some(worktree.path.display().to_string().into())),
            )
        })
        .collect()
}

fn cwd() -> Option<PathBuf> {
    env::current_dir().ok()
}

fn local_branches(cwd: &Path) -> Vec<String> {
    git::discover_branches(cwd).map_or_else(
        |_| Vec::new(),
        |branches| branches.into_iter().map(|record| record.branch).collect(),
    )
}

/// Every branch with a registered worktree, primary included.
fn registered_branches(cwd: &Path) -> HashSet<String> {
    git::repository(cwd).map_or_else(
        |_| HashSet::new(),
        |repository| {
            repository
                .worktrees
                .iter()
                .filter_map(|worktree| match &worktree.kind {
                    WorktreeKind::Branch(branch) => Some(branch.clone()),
                    _ => None,
                })
                .collect()
        },
    )
}

/// Remote-tracking refs whose short name has no local branch of the same name.
/// `origin/feature` alongside a local `feature` is noise: `switch` resolves the
/// local branch first.
fn remote_candidates(cwd: &Path, local: &[String]) -> Vec<CompletionCandidate> {
    let local: HashSet<&str> = local.iter().map(String::as_str).collect();
    git::discover_remote_branches(cwd).map_or_else(
        |_| Vec::new(),
        |remotes| {
            remotes
                .into_iter()
                .filter(|remote| {
                    remote
                        .split_once('/')
                        .is_some_and(|(_, short)| !local.contains(short))
                })
                .map(|remote| CompletionCandidate::new(remote).help(Some("remote branch".into())))
                .collect()
        },
    )
}
```

`CompletionCandidate::new` takes `impl Into<OsString>`, so `&String` works
directly in the `map`. `help` takes `Option<StyledStr>`, and `&str`/`String`
convert into `StyledStr` via `.into()`.

- [ ] **Step 4: Export the module**

In `src/lib.rs`, add alongside the existing `pub mod` declarations, keeping them
alphabetical:

```rust
pub mod completion;
```

- [ ] **Step 5: Attach the candidates**

In `src/main.rs`, add the import:

```rust
use clap_complete::engine::ArgValueCandidates;
```

and add `completion` to the existing `pando::{...}` import list. Then annotate
the three branch arguments in the `Commands` enum:

```rust
    /// Choose, create, or switch to a worktree and print its path.
    Switch {
        /// Branch to switch to; omit it to use the interactive picker.
        #[arg(add = ArgValueCandidates::new(completion::switch_candidates))]
        branch: Option<String>,
        // ... remaining fields unchanged
    },
    /// Create a worktree for a branch and print its path, without confirming a new branch.
    Create {
        /// Branch to create a worktree for.
        #[arg(add = ArgValueCandidates::new(completion::create_candidates))]
        branch: Option<String>,
        // ... remaining fields unchanged
    },
    /// Remove one or more topic worktrees while retaining their branches.
    Remove {
        // ... existing force and dry_run fields unchanged
        #[arg(add = ArgValueCandidates::new(completion::remove_candidates))]
        branches: Vec<String>,
    },
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --test cli -- switch_completes switch_hides create_excludes remove_offers branch_completion`
Expected: PASS, all six.

- [ ] **Step 7: Verify the full suite and lints**

Run: `cargo fmt && cargo clippy --all-targets --all-features -- -D warnings && cargo test --all-features`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml src/completion.rs src/lib.rs src/main.rs tests/cli.rs
git commit -m "feat(completion): complete branch arguments from git"
```

---

### Task 4: Register completion in the managed zsh integration

**Files:**
- Modify: `src/install.rs`
- Test: `tests/cli.rs`

**Interfaces:**
- Consumes: the `COMPLETE=zsh` protocol from Task 1.
- Produces: no Rust API change. `INTEGRATION` stays a `&[u8]` const, so
  `install::run`, `install::preview`, `install::json_plan`, and
  `machine::install` keep their signatures.

- [ ] **Step 1: Write the failing tests**

Append to `tests/cli.rs`:

```rust
#[test]
fn installed_integration_registers_completion_for_both_names() {
    let home = tempfile::tempdir().unwrap();
    let xdg = tempfile::tempdir().unwrap();
    let zdot = tempfile::tempdir().unwrap();

    let mut command = Command::cargo_bin("pando").unwrap();
    let output = run_in_pty(
        command
            .arg("install")
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", xdg.path())
            .env("ZDOTDIR", zdot.path()),
        "y\n",
    );
    assert!(output.status.success());

    let generated =
        fs::read_to_string(xdg.path().join("pando/pando.zsh")).unwrap();
    assert!(generated.contains("COMPLETE=zsh command pando"));
    assert!(generated.contains("compdef _clap_dynamic_completer_pando pd"));
    assert!(
        generated.contains("$+functions[compdef]"),
        "registration must be guarded so it is skipped when compinit has not run"
    );
    assert!(
        !generated.lines().any(|line| line.starts_with('_')),
        "no integration function may use the zsh completion `_name` prefix, \
         which function-table snapshots drop: {generated}"
    );
}
```

`run_in_pty` is the existing PTY helper in `tests/cli.rs`. Match its real name
and signature by reading `install_preserves_zshrc_and_is_idempotent`, which
already drives `install` through a PTY and answers its confirmation prompt.
Reuse that test's exact setup rather than inventing a new one.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test cli installed_integration_registers_completion`
Expected: FAIL. `pando.zsh` has no completion block yet, so the
`COMPLETE=zsh command pando` assertion fails.

- [ ] **Step 3: Implement**

In `src/install.rs`, extend the `INTEGRATION` const. Append this to the end of
the existing raw byte string, after the `pd() { ... }` line and before the
closing `"#`:

```zsh

# Dynamic completion for both names. The registration script is generated on
# every shell start rather than cached: clap_complete gives no stability
# guarantee between the script it emits and the completion protocol the binary
# expects, so a cached script would silently break after a binary upgrade.
# Generating it costs one process spawn, measured under 3ms.
#
# `pd` is a symlink to the same binary, so one registration serves both names.
# The generated function is named `_clap_dynamic_completer_pando`; the
# leading underscore is the completion system's own convention here, unlike the
# dispatcher above, and a dropped snapshot copy only disables completion.
pando_register_completion() {
  (( $+functions[compdef] )) || return 1
  eval "$(COMPLETE=zsh command pando 2>/dev/null)" || return 1
  compdef _clap_dynamic_completer_pando pd
  return 0
}

if [[ -o interactive ]]; then
  if ! pando_register_completion; then
    # `compinit` has not run yet, so retry once after startup finishes.
    pando_deferred_completion() {
      pando_register_completion
      add-zsh-hook -d precmd pando_deferred_completion
      unfunction pando_deferred_completion
    }
    autoload -Uz add-zsh-hook && add-zsh-hook precmd pando_deferred_completion
  fi
fi
```

Note every helper name avoids a leading underscore, preserving the existing
invariant the test asserts.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --test cli installed_integration_registers_completion`
Expected: PASS.

- [ ] **Step 5: Confirm idempotency and the existing install tests still pass**

Run: `cargo test --test cli install`
Expected: PASS. `install_preserves_zshrc_and_is_idempotent` must still pass; the
block is inside the marker-managed `INTEGRATION` const, so rewriting is
idempotent and the second install reports no changes.

- [ ] **Step 6: Verify end to end under real zsh**

Add this test to `tests/cli.rs`. It sources the generated integration under a real
zsh, mirroring the existing `installed_zsh_wt_function_changes_the_invoking_shell_directory`
test, and asserts the completion function is registered for both names.

```rust
#[test]
fn installed_integration_registers_compdef_under_real_zsh() {
    if Command::new("zsh").arg("--version").output().is_err() {
        eprintln!("skipping: zsh is not installed");
        return;
    }

    let home = tempfile::tempdir().unwrap();
    let xdg = tempfile::tempdir().unwrap();
    let zdot = tempfile::tempdir().unwrap();

    // Generate the integration by running a real install, so the test asserts
    // against the shipped bytes rather than a copy that could drift.
    let mut install = Command::cargo_bin("pando").unwrap();
    let installed = run_in_pty(
        install
            .arg("install")
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", xdg.path())
            .env("ZDOTDIR", zdot.path()),
        "y\n",
    );
    assert!(installed.status.success());
    let integration = xdg.path().join("pando/pando.zsh");

    // Put the built binary on PATH so `command pando` resolves during the eval.
    let binary = assert_cmd::cargo::cargo_bin("pando");
    let bin_dir = binary.parent().unwrap();
    let path = format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap());

    let script = format!(
        "autoload -Uz compinit && compinit -u -d {dump}\n\
         source {integration}\n\
         print -r -- \"registered=${{_comps[pando]}} pd=${{_comps[pd]}}\"\n",
        dump = xdg.path().join("zcompdump").display(),
        integration = integration.display(),
    );
    let output = Command::new("zsh")
        .args(["-i", "-c", &script])
        .env("PATH", path)
        .env("HOME", home.path())
        .env("ZDOTDIR", zdot.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("registered=_clap_dynamic_completer_pando"),
        "pando was not registered: {stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("pd=_clap_dynamic_completer_pando"),
        "pd was not registered: {stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
```

`run_in_pty` is the existing PTY helper used by
`installed_zsh_wt_function_changes_the_invoking_shell_directory` and
`install_preserves_zshrc_and_is_idempotent`. Match its real name and signature by
reading those tests; if it differs, adapt the two `install` invocations in this
task and keep the assertions unchanged.

`zsh -i` reads startup files, so `HOME` and `ZDOTDIR` are pointed at temp dirs to
keep the developer's own configuration out of the test. `compinit -u` skips the
insecure-directory prompt that would otherwise block on a temp `HOME`.

- [ ] **Step 7: Verify lints and the full suite**

Run: `cargo fmt && cargo clippy --all-targets --all-features -- -D warnings && cargo test --all-features`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add src/install.rs tests/cli.rs
git commit -m "feat(install): register zsh completion for worktrees and wt"
```

---

### Task 5: Documentation

Required by `.claude/rules/cli-skill-sync.md`: the install surface changed, so the
skill must be updated in the same change.

**Files:**
- Modify: `skills/pando/references/commands/install.md`
- Modify: `skills/pando/SKILL.md`
- Modify: `README.md`
- Modify: `CLAUDE.md`

**Interfaces:**
- Consumes: the behavior built in Tasks 1 through 4.
- Produces: nothing consumed by code.

- [ ] **Step 1: Update the install command reference**

Append this section to `skills/pando/references/commands/install.md`, matching
the heading level the file already uses for its other sections:

```markdown
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
```

- [ ] **Step 2: Update SKILL.md**

In `skills/pando/SKILL.md`, find the `install` entry in the command table and
extend its description so it reads:

```markdown
| `install` | Install the managed zsh integration: the `pd`/`pando` shell functions that change directory, and dynamic tab completion for subcommands, flags, and branch arguments. |
```

Keep the surrounding table formatting as it is. If the existing description is
worded differently, preserve its wording and append the completion clause rather
than replacing the row wholesale.

- [ ] **Step 3: Update the README**

In `README.md`, add this immediately after the section that documents
`pando install`:

```markdown
### Tab completion

The zsh integration also registers tab completion for `pando` and `pd`.
Branch arguments complete from the current repository: `switch` offers local and
remote-tracking branches, `create` offers branches without a worktree, and
`remove` offers only branches that have one.

Completion arrives with the integration, so restart zsh or re-source your
`.zshrc` after installing.
```

- [ ] **Step 4: Update the module map**

In `CLAUDE.md`, add a row to the module map table:

```markdown
| `completion.rs` | Best-effort candidate producers for dynamic zsh completion of branch arguments |
```

`CLAUDE.md` is the real file and `AGENTS.md` is a symlink to it, so editing
`CLAUDE.md` updates both.

- [ ] **Step 5: Verify the docs match the build**

Run: `cargo test --all-features`
Expected: PASS. Then reread each edited file and confirm no claim contradicts the
tests written in Tasks 1 through 4, particularly that `remove` does **not**
exclude branches already typed on the command line.

- [ ] **Step 6: Commit**

```bash
git add skills/pando README.md CLAUDE.md
git commit -m "docs: document zsh completion in the install surface"
```

---

## Known limitations to carry into the docs

- `remove` re-offers branches already present on the command line. clap's
  candidate API receives no prior-word context, so deduplicating would require
  hand-written zsh.
- Only zsh is registered. `CompleteEnv` supports bash and fish, but
  `pando install` manages zsh alone.
- Completion depends on `compinit` having run. The `precmd` fallback in Task 4
  covers the common case where a user's `.zshrc` calls `compinit` after the
  pando block.
