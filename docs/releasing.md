# Releasing

Releases are driven by [release-please](https://github.com/googleapis/release-please) from Conventional Commit messages on `main`.

1. Every push to `main` runs `.github/workflows/release.yml`. When releasable commits (`feat:`, `fix:`, `deps:`, or any `!` breaking change) have landed since the last release, release-please opens or refreshes a release PR titled `chore(main): release X.Y.Z`. It bumps the version in `Cargo.toml`, `Cargo.lock`, `flake.nix`, and `.release-please-manifest.json`, and prepends the generated entry to `CHANGELOG.md`.
2. Merging that PR tags `vX.Y.Z`, creates the GitHub release, and attaches prebuilt `pando` archives with SHA-256 checksums for `x86_64-unknown-linux-gnu` and `aarch64-apple-darwin`.
3. The crate is not published to crates.io; `Cargo.toml` sets `publish = false` so `cargo publish` refuses to run.

While the project is pre-1.0, `feat:` and breaking changes bump the minor version and `fix:` bumps the patch version. To force a specific version, land a commit with a `Release-As: X.Y.Z` footer.

`RELEASE_NOTES.md` stays hand-written for narrative upgrade notes; `CHANGELOG.md` is generated, so edit a release's entry in the release PR rather than after it merges.

## Repository settings

- **Settings > Actions > General > Workflow permissions**: enable "Allow GitHub Actions to create and approve pull requests" so release-please can open its PR.
- **`RELEASE_PLEASE_TOKEN` secret (recommended)**: a fine-grained personal access token or GitHub App token with Contents and Pull requests read/write on this repository. PRs opened with the default `GITHUB_TOKEN` do not trigger CI, so without it the release PR shows no checks.
