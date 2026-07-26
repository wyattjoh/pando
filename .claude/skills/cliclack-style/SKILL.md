---
name: cliclack-style
description: Guides consistent Cliclack terminal UX in this Rust CLI. Use when adding or changing Cliclack prompts, logs, spinners, progress, themes, picker rendering, cancellation, or terminal-width behavior.
---

# Cliclack Style

Apply this guidance when changing human-facing terminal UI. This project uses Cliclack `0.5.5` and its shared UI facade is `src/ui.rs`.

## Start With Project Conventions

1. Read `src/ui.rs` and the affected command flow before changing UI.
2. Keep machine-readable stdout pure: successful `switch` writes only its destination and `get` writes only its requested property. Prompts, logs, warnings, and hook output belong on stderr.
3. Install the shared theme once at application startup. Use the semantic `ui` wrappers and styles; do not set themes per command.
4. Preserve the current project's color meanings: accent for active/successful flow, muted for supporting metadata, warning for caution, error for failure, and selected for the active choice.

## Flow and Output Order

For a substantial interactive flow, use this order:

1. Optional `intro` only when the flow has multiple meaningful stages.
2. Short, sequential prompts with safe/common defaults.
3. `step`, `info`, or a spinner/progress bar while work proceeds.
4. Optional `note` for multi-line follow-up or next steps.
5. Exactly one final `outro`/`ui::finish` on successful completion.

Do not add an intro/banner to a simple status command. Do not routinely clear the terminal: Cliclack's `clear_screen` affects both stdout and stderr.

Use semantic output deliberately:

- `info` for facts and guidance.
- `step` for a completed substep.
- `success` for a completed success.
- `warning` for caution, a no-op, or a user-declined action.
- `error` for actual failures.
- `note`/`outro_note` for structured, multi-line follow-up.

## Prompts, Errors, and Cancellation

- Make prompt labels concise and imperative; put secondary explanation in hints or notes.
- Use prompt validation for recoverable input mistakes instead of aborting the command.
- Treat `Interrupted` (Esc/Ctrl-C) as cancellation, not an I/O/internal error. Return a concise `<operation> cancelled` result and do not emit a success outro afterward.
- Every spinner or progress bar must reach exactly one terminal state: stop, cancel, error, or clear. Close it before reporting cancellation or failure.
- Use a spinner for indeterminate work. Use progress only when a meaningful total exists; use multi-progress for concurrent work.

## Layout and Responsive Rendering

- Use Cliclack's standard prompts where possible. Cap long select/multiselect lists with `.max_rows(...)` so they scroll.
- Use a note for long explanatory text; its formatter wraps to terminal width.
- Keep ordinary labels, logs, and path displays compact. Standard select labels are not automatically made safe for arbitrary long content.
- A custom redraw loop must measure visible display width (after stripping ANSI), keep every rendered logical line within the current stderr width, and account for every physical row when clearing/redrawing. Truncate with an ellipsis rather than allowing a row to wrap.
- Prefer Cliclack/theme-owned framing. Only render `│`/`└` manually when implementing a custom interactive renderer that Cliclack cannot provide; then preserve the shared theme, keyboard behavior, cancellation semantics, and narrow-terminal tests.

## Verification

When UI behavior changes, add or update a PTY integration test that covers:

- stdout/stderr separation and exit status;
- default, cancellation, and error paths as applicable;
- ANSI semantic styling where it conveys state;
- a constrained terminal for scrolling or custom rendering;
- width safety for long labels, paths, and help text.

Run `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --all-features`.

## Upstream References

Base Cliclack guidance on the version used by this repository, not `main`:

- [Cliclack 0.5.5 source](https://docs.rs/crate/cliclack/0.5.5/source/)
- [Basic flow example](https://github.com/fadeevab/cliclack/blob/ae7b17d8ceeea736f7725d494d0f37e4a865cb30/examples/basic.rs)
- [Validation example](https://github.com/fadeevab/cliclack/blob/ae7b17d8ceeea736f7725d494d0f37e4a865cb30/examples/validation.rs)
- [Spinner example](https://github.com/fadeevab/cliclack/blob/ae7b17d8ceeea736f7725d494d0f37e4a865cb30/examples/spinner.rs)
- [Scrollable list example](https://github.com/fadeevab/cliclack/blob/ae7b17d8ceeea736f7725d494d0f37e4a865cb30/examples/max_rows.rs)
