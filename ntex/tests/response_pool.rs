use ntex::http::{Response, StatusCode};

#[test]
fn response_extensions_are_cleared_when_reused_from_pool() {
    let resp = Response::new(StatusCode::OK);
    resp.extensions_mut().insert(42usize);
    drop(resp);

    let resp = Response::new(StatusCode::OK);
    assert!(resp.extensions().get::<usize>().is_none());
}
