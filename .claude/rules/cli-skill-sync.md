---
description: Keep the skills/worktrees usage skill in sync with the CLI's public surface (commands, flags, config schema, JSON contract)
paths:
  - "src/main.rs"
  - "src/smart.rs"
  - "src/commit.rs"
  - "src/protocol.rs"
  - "src/machine.rs"
  - "src/install.rs"
  - "src/lifecycle.rs"
  - "src/config.rs"
  - "README.md"
alwaysApply: false
---

# CLI skill sync

`skills/worktrees/` (symlinked into `.claude/skills/worktrees` and, via that
directory's existing symlink, into `.agents/skills/worktrees`) is a
CLI-usage skill for `worktrees` itself, generated with the `new-cli-skill`
skill. It documents the command surface, flags, config schema, and JSON
contract as a kickstart reference — it is not derived automatically.

When a change touches any of this rule's paths in a way that changes the
CLI's **public surface**, update the matching `skills/worktrees/` file in
the same change:

| Surface change | Update |
|---|---|
| A top-level command, subcommand, or flag is added, renamed, or removed (`src/main.rs`, `src/smart.rs`'s `GetProperty`/`TrustCommand` enums) | `skills/worktrees/SKILL.md`'s command table and the relevant file under `skills/worktrees/references/commands/` |
| The shared JSON protocol changes (`src/protocol.rs`, `src/machine.rs`, or a command-owning module) | `skills/worktrees/SKILL.md` and every affected command reference |
| A config key, hook phase, or layering rule changes (`src/config.rs`) | `skills/worktrees/references/config.md` |
| A documented workflow in `README.md` changes | the matching "Common workflows" entry in `skills/worktrees/SKILL.md` |

Regenerating from scratch is rarely necessary — most changes are a targeted
edit to one reference file. Re-run the `new-cli-skill` skill (offering
"Update in place") only after a surface change large enough that a targeted
edit would leave the skill inconsistent.
