//! WebSocket opening-handshake helpers.
use base64::{Engine, engine::general_purpose::STANDARD as base64};

use crate::http::header::HeaderName;
use crate::http::{HeaderMap, RequestHead, Response, ResponseBuilder};
use crate::http::{Method, StatusCode, header};

use super::error::HandshakeError;

/// Verifies a WebSocket opening-handshake request and creates its response.
///
/// # Errors
///
/// Returns [`HandshakeError`] when the request method or required upgrade
/// headers are invalid.
pub fn handshake(req: &RequestHead) -> Result<ResponseBuilder, HandshakeError> {
    verify_handshake(req)?;
    Ok(handshake_response(req))
}

/// Verifies a WebSocket opening-handshake request.
///
/// The request must use `GET`, request a connection upgrade to WebSocket,
/// include a `Sec-WebSocket-Key`, and use WebSocket version 7, 8, or 13.
pub fn verify_handshake(req: &RequestHead) -> Result<(), HandshakeError> {
    // WebSocket accepts only GET
    if req.method != Method::GET {
        return Err(HandshakeError::GetMethodRequired);
    }

    // Check for "UPGRADE" to websocket header
    if !header_contains_token(req.headers(), &header::UPGRADE, "websocket") {
        return Err(HandshakeError::NoWebsocketUpgrade);
    }

    // Upgrade connection
    if !header_contains_token(req.headers(), &header::CONNECTION, "upgrade") {
        return Err(HandshakeError::NoConnectionUpgrade);
    }

    // check supported version
    if !req.headers().contains_key(header::SEC_WEBSOCKET_VERSION) {
        return Err(HandshakeError::NoVersionHeader);
    }
    let supported_ver = {
        if let Some(hdr) = req.headers().get(header::SEC_WEBSOCKET_VERSION) {
            hdr == "13" || hdr == "8" || hdr == "7"
        } else {
            false
        }
    };
    if !supported_ver {
        return Err(HandshakeError::UnsupportedVersion);
    }

    // check client handshake for validity
    let mut keys = req.headers().get_all(header::SEC_WEBSOCKET_KEY);
    let valid_key = keys
        .next()
        .and_then(|key| base64.decode(key.as_bytes()).ok())
        .is_some_and(|key| key.len() == 16)
        && keys.next().is_none();
    if !valid_key {
        return Err(HandshakeError::BadWebsocketKey);
    }
    Ok(())
}

pub(super) fn header_contains_token(
    headers: &HeaderMap,
    name: &HeaderName,
    expected: &str,
) -> bool {
    headers.get_all(name).any(|value| {
        value.to_str().is_ok_and(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case(expected))
        })
    })
}

/// Creates a WebSocket opening-handshake response.
///
/// The returned response builder has status `101 Switching Protocols` and the
/// required upgrade and challenge-response headers.
///
/// # Panics
///
/// Panics if `req` does not contain a `Sec-WebSocket-Key` header or the key
/// exceeds the length accepted by [`hash_key`](crate::ws::hash_key). Use
/// [`handshake`] when the request has not already been validated.
pub fn handshake_response(req: &RequestHead) -> ResponseBuilder {
    let key = {
        let key = req.headers().get(header::SEC_WEBSOCKET_KEY).unwrap();
        crate::ws::hash_key(key.as_ref()).expect("validated Sec-WebSocket-Key")
    };

    Response::builder(StatusCode::SWITCHING_PROTOCOLS)
        .upgrade("websocket")
        .header(header::SEC_WEBSOCKET_ACCEPT, key)
        .take()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{error::ResponseError, test::TestRequest};

    #[test]
    fn test_handshake() {
        let req = TestRequest::default().method(Method::POST).build();
        assert_eq!(
            HandshakeError::GetMethodRequired,
            verify_handshake(req.head()).err().unwrap()
        );

        let req = TestRequest::default().build();
        assert_eq!(
            HandshakeError::NoWebsocketUpgrade,
            verify_handshake(req.head()).err().unwrap()
        );

        let req = TestRequest::default()
            .header(header::UPGRADE, header::HeaderValue::from_static("test"))
            .build();
        assert_eq!(
            HandshakeError::NoWebsocketUpgrade,
            verify_handshake(req.head()).err().unwrap()
        );

        let req = TestRequest::default()
            .header(
                header::UPGRADE,
                header::HeaderValue::from_static("notwebsocket"),
            )
            .build();
        assert_eq!(
            HandshakeError::NoWebsocketUpgrade,
            verify_handshake(req.head()).err().unwrap()
        );

        let req = TestRequest::default()
            .header(
                header::UPGRADE,
                header::HeaderValue::from_static("WebSocket"),
            )
            .build();
        assert_eq!(
            HandshakeError::NoConnectionUpgrade,
            verify_handshake(req.head()).err().unwrap()
        );

        let req = TestRequest::default()
            .header(
                header::UPGRADE,
                header::HeaderValue::from_static("websocket"),
            )
            .header(
                header::CONNECTION,
                header::HeaderValue::from_static("keep-alive, Upgrade"),
            )
            .build();
        assert_eq!(
            HandshakeError::NoVersionHeader,
            verify_handshake(req.head()).err().unwrap()
        );

        let req = TestRequest::default()
            .header(
                header::UPGRADE,
                header::HeaderValue::from_static("websocket"),
            )
            .header(
                header::CONNECTION,
                header::HeaderValue::from_static("keep-alive, upgraded"),
            )
            .build();
        assert_eq!(
            HandshakeError::NoConnectionUpgrade,
            verify_handshake(req.head()).err().unwrap()
        );

        let req = TestRequest::default()
            .header(
                header::UPGRADE,
                header::HeaderValue::from_static("websocket"),
            )
            .header(
                header::CONNECTION,
                header::HeaderValue::from_static("upgrade"),
            )
            .header(
                header::SEC_WEBSOCKET_VERSION,
                header::HeaderValue::from_static("5"),
            )
            .build();
        assert_eq!(
            HandshakeError::UnsupportedVersion,
            verify_handshake(req.head()).err().unwrap()
        );

        let req = TestRequest::default()
            .header(
                header::UPGRADE,
                header::HeaderValue::from_static("websocket"),
            )
            .header(
                header::CONNECTION,
                header::HeaderValue::from_static("upgrade"),
            )
            .header(
                header::SEC_WEBSOCKET_VERSION,
                header::HeaderValue::from_static("13"),
            )
            .build();
        assert_eq!(
            HandshakeError::BadWebsocketKey,
            verify_handshake(req.head()).err().unwrap()
        );

        let req = TestRequest::default()
            .header(
                header::UPGRADE,
                header::HeaderValue::from_static("websocket"),
            )
            .header(
                header::CONNECTION,
                header::HeaderValue::from_static("upgrade"),
            )
            .header(
                header::SEC_WEBSOCKET_VERSION,
                header::HeaderValue::from_static("13"),
            )
            .header(
                header::SEC_WEBSOCKET_KEY,
                header::HeaderValue::from_static("13"),
            )
            .build();
        assert_eq!(
            HandshakeError::BadWebsocketKey,
            verify_handshake(req.head()).err().unwrap()
        );

        let req = TestRequest::default()
            .header(
                header::UPGRADE,
                header::HeaderValue::from_static("websocket"),
            )
            .header(
                header::CONNECTION,
                header::HeaderValue::from_static("upgrade"),
            )
            .header(
                header::SEC_WEBSOCKET_VERSION,
                header::HeaderValue::from_static("13"),
            )
            .header(
                header::SEC_WEBSOCKET_KEY,
                header::HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
            )
            .build();
        verify_handshake(req.head()).unwrap();
        let response = handshake_response(req.head()).build();
        assert_eq!(StatusCode::SWITCHING_PROTOCOLS, response.status());
        assert!(!response.headers().contains_key(header::TRANSFER_ENCODING));
    }

    #[test]
    fn test_wserror_http_response() {
        let resp: Response = HandshakeError::GetMethodRequired.error_response();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        let resp: Response = HandshakeError::NoWebsocketUpgrade.error_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let resp: Response = HandshakeError::NoConnectionUpgrade.error_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let resp: Response = HandshakeError::NoVersionHeader.error_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let resp: Response = HandshakeError::UnsupportedVersion.error_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let resp: Response = HandshakeError::BadWebsocketKey.error_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let resp: Response = HandshakeError::BadWebsocketProtocol.error_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
