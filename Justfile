# Build the project.
build:
    cargo build --all-features

# Run the test suite.
test:
    cargo test --all-features

# Install the binary from this checkout as `worktrees` and `wt`.
install:
    cargo install --path .
    install_root="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}"; ln -sfn worktrees "$install_root/bin/wt"
