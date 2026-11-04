use ntex_service::{fn_service, service};

/// STATEMENT: Chain construction rejects mismatched service error types.
///
/// HOW:
/// - Append a service with a different error type using `and_then` and `then`.
/// - Discard both chains without constructing pipelines or calling services.
fn main() {
    let _ = service::<_, (), _>(fn_service(async |value: u32| Ok::<_, ()>(value)))
        .and_then(fn_service(async |value: u32| Ok::<_, &'static str>(value)));

    let _ = service::<_, (), _>(fn_service(async |value: u32| Ok::<_, ()>(value))).then(
        fn_service(async |result: Result<u32, ()>| Ok::<_, &'static str>(result.unwrap_or(0))),
    );
}
