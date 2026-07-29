# `worktrees pr create`

Create a draft pull request from the current topic branch. The branch is published with an ordinary fast-forward-safe push before GitHub creates the pull request. When either title or description is omitted, `pr.generation.command` must be configured. Supplying both `--title` and `--description` explicitly bypasses generator configuration and trust checks. Missing generator configuration is rejected during initial preflight, before dirty-worktree Commit/Skip/Yolo handling or other local mutation.

```sh
worktrees pr create --title "Title" --description "Body"
worktrees pr create --title "Title" --description-file body.md --status ready
```

When metadata is generated, `pr.pull-request-template` may provide a personal or project template. Template precedence is local configuration, shared project configuration, committed `.github/pull_request_template.md` (then uppercase fallback), and global configuration. Repository templates are read from committed `HEAD`, never from dirty working-tree files. The generator prompt's own `pr.generation.template` precedence is separate; when configured, it controls the complete prompt and the `pull_request_template` variable is inserted only where requested. Without it, the built-in prompt always includes the resolved template and asks the generator to preserve required headings, checklists, and sections while replacing placeholders and comments.

The generator is invoked up to three times per run. Any failure — a nonzero exit or a document that is not a single level-one heading followed by a non-empty description — is retried, with the previous rejection appended to the prompt so the generator can correct itself. Retries are silent; the failure after the third attempt reports the attempt count. Leading blank lines and a code fence wrapping the whole document are tolerated rather than retried.

The configured `worktrees.target-branch` is used when present. When omitted, the base falls back to the local branch pointed to by `origin/HEAD`, then local `main`, then local `master`. An existing upstream is preferred, otherwise `origin` is used, or the sole configured remote. Ambiguous remote selection fails rather than prompting. Use `--dry-run` to inspect the planned push without changing refs, and `--force` for non-interactive human execution. The `--force` option never force-pushes. JSON output returns one typed response document; JSON request mode accepts `title`, `description`, `description_file`, `status`, and `dry_run`. `description_file: "-"` is supported for human and direct JSON output, but not request mode.
