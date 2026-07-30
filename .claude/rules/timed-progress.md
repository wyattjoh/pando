---
description: Use the shared timed progress wrapper for slow human-mode operations
paths:
  - "src/**/*.rs"
alwaysApply: false
---

Use `ui::run_timed` for indeterminate operations that capture their output and may take noticeable time:

```rust
let result = ui::run_timed(
    human_mode,
    "Publishing topic branch...",
    "Published topic branch",
    "Failed to publish topic branch",
    |animated| operation(!animated),
)?;
```

Pass `false` for JSON or other machine-only execution so the wrapper emits nothing. The closure receives `animated = true` when the spinner owns stderr. Capture subprocess output in that case; inherited output would corrupt the spinner. Preserve inherited output when `animated = false` if the subprocess provides useful progress or authentication interaction.

Do not wrap prompts, editors, or configured hooks. Never nest progress indicators.

A wrapped subprocess that could reach for an editor must be denied one. The lifecycle Git operations in `git::run_lifecycle_git` set `GIT_EDITOR=true` on the captured child: `GIT_EDITOR` outranks `core.editor`, so an inherited `EDITOR=nvim` would otherwise leave `git rebase --continue` drawing a full-screen editor into a pipe.

Captured Git output belongs in the rail, not on raw stderr. Return the transcript from the wrapped call and render it with `render::git_output`, which mutes Git's prose and highlights diffstat paths. Fold the same transcript into the error on failure so conflicts stay readable. Strip Git's carriage-return progress redraws — a pipe preserves them where a terminal would have overwritten the line. `ui::run_timed` owns the timer and exactly one success or failure terminal state, so callers must not render duplicate progress messages around it.
