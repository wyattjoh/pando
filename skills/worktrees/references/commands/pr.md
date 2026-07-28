# `worktrees pr create`

Create a draft pull request from the current clean topic branch. The branch is published with an ordinary fast-forward-safe push before GitHub creates the pull request.

```sh
worktrees pr create --title "Title" --description "Body"
worktrees pr create --title "Title" --description-file body.md --status ready
```

The configured `worktrees.target-branch` is always used as the base. An existing upstream is preferred, otherwise `origin` is used, or the sole configured remote. Ambiguous remote selection fails rather than prompting. Use `--dry-run` to inspect the planned push without changing refs, and `--force` for non-interactive human execution. The `--force` option never force-pushes. JSON output returns one typed response document; JSON request mode accepts `title`, `description`, `description_file`, `status`, and `dry_run`. `description_file: "-"` is supported for human and direct JSON output, but not request mode.
