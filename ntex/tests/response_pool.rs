use ntex::http::{Response, StatusCode};

/// STATEMENT: A pooled `Response` does not retain extensions from its previous use.
///
/// HOW:
/// - Insert a `usize` extension into an OK response and drop it.
/// - Create another OK response and assert its extensions contain no `usize`.
#[test]
fn response_extensions_are_cleared_when_reused_from_pool() {
    let resp = Response::new(StatusCode::OK);
    resp.extensions_mut().insert(42usize);
    drop(resp);

    let resp = Response::new(StatusCode::OK);
    assert!(resp.extensions().get::<usize>().is_none());
}
