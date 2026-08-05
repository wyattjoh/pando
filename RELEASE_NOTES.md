# Release notes

## 0.2.0

### Breaking Rust library changes

The setup implementation is now crate-private. The public `pando::setup` module and its `PendingRecord`, `marker_path`, `prepare`, `is_incomplete`, and `clear` symbols have been removed. Post-create setup state is now managed internally through consuming lifecycle handles.

The command-line interface, JSON version 1 protocol, configuration, durable setup record format, and shell integration remain compatible.
