# Release notes

## 0.2.0

### JSON schema precision

Generated exact-leaf help now describes the existing version 1 wire literally: request and response `schema_version` fields accept only the integer `1`, and response `status` accepts only `"success"` or `"error"`. This corrects the published schemas without changing runtime serialization.

### Breaking Rust library changes

The setup implementation is now crate-private. The public `pando::setup` module and its `PendingRecord`, `marker_path`, `prepare`, `is_incomplete`, and `clear` symbols have been removed. Post-create setup state is now managed internally through consuming lifecycle handles.

Superseded caller-driven lifecycle choreography is also removed rather than retained behind compatibility wrappers. Merge execution now owns journaled preparation, execution, recovery, and final outcomes; squash preparation is an opaque internal capsule; setup transitions use consuming internal handles; and guided installation executes command-owned proposals. Former setup output-policy types, caller-visible squash plans, split merge execution and cleanup seams, and adapter-owned install outcomes are no longer Rust interfaces.

The command-line interface, JSON version 1 protocol, configuration, durable setup and merge journal formats, and shell integration remain compatible.
