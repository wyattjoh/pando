# Build the project.
build:
    cargo build --all-features

# Check formatting and run Clippy with the same settings as CI.
lint:
    cargo fmt --check
    cargo clippy --all-targets --all-features -- -D warnings

# Run the test suite.
test:
    cargo test --all-features

# Install the pre-commit hooks managed by prek.
install-hooks:
    prek install

# Install the binary from this checkout as `pando` and `pd`.
install:
    cargo install --locked --path .
    install_root="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}"; ln -sfn pando "$install_root/bin/pd"
