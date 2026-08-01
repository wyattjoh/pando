# Build the project.
build:
    cargo build --all-features

# Run the test suite.
test:
    cargo test --all-features

# Install the binary from this checkout as `pando` and `pd`.
install:
    cargo install --locked --path .
    install_root="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}"; ln -sfn pando "$install_root/bin/pd"
