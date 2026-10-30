# Repository instructions

## Test conventions

### Test documentation

Document every test, including ignored and conditionally compiled tests, with
rustdoc immediately above its test attributes:

```rust
/// STATEMENT: A retired service is rebound rather than served from a stale handle.
///
/// HOW:
/// - Resolve the service, then retire that incarnation.
/// - Call again and confirm the handle is rebound instead of reused.
#[ntex::test]
async fn a_retired_service_is_rebound() {
    // Test implementation.
}
```

- `STATEMENT:` describes the behavior or invariant the test verifies.
- `HOW:` describes the concrete setup, actions, and observations used to verify
  it. Match the actual assertions; do not merely restate the test name.
- Use backticks around code identifiers in rustdoc.
