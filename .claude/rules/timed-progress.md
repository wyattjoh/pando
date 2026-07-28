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

Do not wrap prompts, editors, configured hooks, or Git operations that intentionally inherit interactive output. Never nest progress indicators. `ui::run_timed` owns the timer and exactly one success or failure terminal state, so callers must not render duplicate progress messages around it.
