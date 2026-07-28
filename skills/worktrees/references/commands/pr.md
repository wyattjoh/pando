# `worktrees pr create`

Create a draft pull request from the current clean, published topic branch.

```sh
worktrees pr create --title "Title" --description "Body"
worktrees pr create --title "Title" --description-file body.md --status ready
```

The configured `worktrees.target-branch` is always used as the base. Use `--dry-run` to inspect the planned effect and `--force` for non-interactive human execution. JSON output returns one typed response document; JSON request mode accepts `title`, `description`, `description_file`, `status`, and `dry_run`. `description_file: "-"` is supported for human and direct JSON output, but not request mode.
