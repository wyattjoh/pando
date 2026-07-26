# Build the project.
build:
    cargo build --all-features

# Run the test suite.
test:
    cargo test --all-features

# Install the binary from this checkout.
install:
    cargo install --path .
