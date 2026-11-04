# Repository instructions

These instructions are for working on ntex itself. Guidance for code that
*uses* ntex is in `docs/llms.txt`, published at <https://docs.ntex.rs/llms.txt>.

## Layout

Cargo workspace. `ntex` is the umbrella crate that re-exports the others:

- `ntex-service`: `Service`, `ServiceFactory`, `Middleware`, `Pipeline`, `forward!`
- `ntex-util`: service combinators (in-flight limit, keep-alive, timeouts), futures, channels, timers
- `ntex-io`, `ntex-codec`, `ntex-net`, `ntex-tls`: I/O, framing, connectors, TLS filters
- `ntex-server`, `ntex-rt`, `ntex-dispatcher`: server, runtime, protocol dispatcher
- `ntex-http`, `ntex-router`, `ntex-bytes`, `ntex-error`, `ntex-macros`, `urly`: support crates
- `docs/`: the guide published at <https://docs.ntex.rs>

## Commands

`ntex` requires one runtime feature. The CI feature set is:

```sh
FEATURES="ntex/tokio,ntex/cookie,ntex/compress,ntex/openssl,ntex/rustls,ntex/ws"

cargo fmt --all -- --check
cargo +stable clippy --tests --all --no-default-features --features="$FEATURES" -- -D warnings
cargo nextest run --all --no-default-features --features="$FEATURES"
cargo test --doc --all --no-default-features --features="$FEATURES"
```

For a single crate, prefer the smallest command, for example
`cargo test -p ntex-service` or `cargo test -p ntex --features tokio --lib web::app`.

Compiler diagnostics for common `ntex-service` mistakes are snapshot-tested on
stable. After changing a diagnostic or `forward!`, regenerate and review
`ntex-service/tests/ui/*.stderr`:

```sh
TRYBUILD=overwrite cargo +stable test -p ntex-service --test compile_fail -- --ignored
```

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

## Service wrappers

A service that wraps child services must forward `Service::bind`,
`Service::ready`, and `Service::shutdown` to every child. A missing `bind`
loses readiness notifications. A missing `ready` hides backpressure from the
parent. A missing `shutdown` skips the child's graceful shutdown.

- Use `forward!(St, field, ...)` for plain wrappers, where `St` is the state
  type of the impl. Add `map_err = f` when the readiness error must be
  converted. Inside the `ntex` crate write `crate::forward!`; in other
  workspace crates write `ntex_service::forward!`.
- Do not use the hidden `forward_bind!`, `forward_ready!`, or
  `forward_shutdown!` macros in new code.
- Implement the methods explicitly with `BindCtx` and `Ctx` when the wrapper
  has extra readiness sources (`BindCtx::subscribe`, `BindCtx::on_notify`),
  when it intentionally skips forwarding one of the methods, or when a child is a
  `Pipeline` (`Pipeline::bind_to`, `Pipeline::ready`, `Pipeline::shutdown`).
  Comment intentional skips.
- Call child services through `Ctx::call` so readiness and binding are
  preserved.
- Prefer `web::middleware::from_fn` for web middleware and `fn_layer` for
  other services when the middleware only runs code before or after the inner
  service; both forward binding, readiness, and shutdown themselves.

## Changelog

Add user-visible changes to the crate's `CHANGES.md` under the current
unreleased version. Do not bump versions unless asked.
