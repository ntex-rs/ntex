//! HTTP client errors.
use std::{error::Error as StdError, io, ops::Deref, rc::Rc};

use serde_json::error::Error as JsonError;

use crate::error::ErrorDiagnostic;
use crate::http::error::{DecodeError, EncodeError, HttpError, PayloadError};
use crate::util::{Either, clone_io_error};

/// Errors that can occur while deserializing a JSON response payload.
#[derive(thiserror::Error, Debug)]
pub enum JsonPayloadError {
    /// The response content type is not JSON.
    #[error("Content type error")]
    ContentType,
    /// The response body could not be deserialized.
    #[error("Json deserialize error")]
    Deserialize(#[source] Option<JsonError>),
    /// The response payload could not be read.
    #[error("Error occurred while reading payload")]
    Payload(
        #[from]
        #[source]
        ClientPayloadError,
    ),
}

impl Clone for JsonPayloadError {
    fn clone(&self) -> Self {
        match self {
            JsonPayloadError::ContentType => JsonPayloadError::ContentType,
            JsonPayloadError::Deserialize(_) => JsonPayloadError::Deserialize(None),
            JsonPayloadError::Payload(err) => JsonPayloadError::Payload(err.clone()),
        }
    }
}

impl From<JsonError> for JsonPayloadError {
    fn from(err: JsonError) -> JsonPayloadError {
        JsonPayloadError::Deserialize(Some(err))
    }
}

impl From<PayloadError> for JsonPayloadError {
    fn from(err: PayloadError) -> JsonPayloadError {
        JsonPayloadError::Payload(ClientPayloadError(err))
    }
}

impl ErrorDiagnostic for JsonPayloadError {
    fn signature(&self) -> &'static str {
        match self {
            JsonPayloadError::ContentType => "ntex-client-JsonContentType",
            JsonPayloadError::Deserialize(_) => "ntex-client-JsonDeserialize",
            JsonPayloadError::Payload(_) => "ntex-client-JsonPayload",
        }
    }
}

#[derive(thiserror::Error, Clone, Debug)]
#[error("{0}")]
/// Error returned while reading a client response payload.
pub struct ClientPayloadError(
    #[from]
    #[source]
    pub(crate) PayloadError,
);

impl Deref for ClientPayloadError {
    type Target = PayloadError;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl ErrorDiagnostic for ClientPayloadError {
    fn signature(&self) -> &'static str {
        "ntex-client-Payload"
    }
}

/// Errors that can occur while connecting to an HTTP host.
#[derive(thiserror::Error, Debug)]
pub enum ConnectError {
    /// TLS support is not enabled.
    #[error("SSL is not supported")]
    SslIsNotSupported,

    /// The host name could not be resolved.
    #[error("Failed resolving hostname: {0}")]
    Resolver(
        #[from]
        #[source]
        io::Error,
    ),

    /// No DNS records were found.
    #[error("No dns records found for the input")]
    NoRecords,

    /// The connector disconnected.
    #[error("Connector has been disconnected")]
    Disconnected(#[source] Option<io::Error>),

    /// The connector received invalid input.
    #[error("Invalid connect input")]
    InvalidInput,

    /// The connector received an unresolved host name.
    #[error("Connector received `Connect` method with unresolved host")]
    Unresolved,
}

impl ErrorDiagnostic for ConnectError {
    fn signature(&self) -> &'static str {
        match self {
            ConnectError::SslIsNotSupported => "ntex-client-connect-SslIsNotSupported",
            ConnectError::Resolver(..) => "ntex-client-connect-Resolver",
            ConnectError::NoRecords => "ntex-client-connect-NoRecords",
            ConnectError::Disconnected(_) => "ntex-client-connect-Disconnected",
            ConnectError::InvalidInput => "ntex-client-connect-InvalidInput",
            ConnectError::Unresolved => "ntex-client-connect-Unresolved",
        }
    }
}

impl Clone for ConnectError {
    fn clone(&self) -> Self {
        match self {
            ConnectError::SslIsNotSupported => ConnectError::SslIsNotSupported,
            ConnectError::Resolver(e) => ConnectError::Resolver(clone_io_error(e)),
            ConnectError::NoRecords => ConnectError::NoRecords,
            ConnectError::Disconnected(e) => {
                if let Some(e) = e {
                    ConnectError::Disconnected(Some(clone_io_error(e)))
                } else {
                    ConnectError::Disconnected(None)
                }
            }
            ConnectError::InvalidInput => ConnectError::InvalidInput,
            ConnectError::Unresolved => ConnectError::Unresolved,
        }
    }
}

impl From<crate::connect::ConnectError> for ConnectError {
    fn from(err: crate::connect::ConnectError) -> ConnectError {
        match err {
            crate::connect::ConnectError::Resolver(e) => ConnectError::Resolver(e),
            crate::connect::ConnectError::NoRecords => ConnectError::NoRecords,
            crate::connect::ConnectError::InvalidInput => ConnectError::InvalidInput,
            crate::connect::ConnectError::Unresolved => ConnectError::Unresolved,
            crate::connect::ConnectError::Io(e) => ConnectError::Disconnected(Some(e)),
        }
    }
}

#[derive(Copy, Clone, Debug, thiserror::Error)]
/// Error returned when a request URI is not valid for an HTTP client request.
pub enum InvalidUrl {
    /// The URI does not contain a scheme.
    #[error("Missing url scheme")]
    MissingScheme,
    /// The URI uses a scheme other than HTTP, HTTPS, WS, or WSS.
    #[error("Unknown url scheme")]
    UnknownScheme,
    /// The URI does not contain a host.
    #[error("Missing host name")]
    MissingHost,
    /// The URI could not be parsed or constructed.
    #[error("Url parse error: {0}")]
    Http(
        #[from]
        #[source]
        HttpError,
    ),
}

/// Errors that can occur while sending a request or reading its response.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// Invalid URL
    #[error("Invalid URL: {0}")]
    Url(
        #[from]
        #[source]
        InvalidUrl,
    ),
    /// Failed to connect to host
    #[error("Failed to connect to host: {0}")]
    Connect(
        #[from]
        #[source]
        ConnectError,
    ),
    /// Error sending request
    #[error("Error sending request: {0}")]
    Send(
        #[from]
        #[source]
        io::Error,
    ),
    /// Error encoding request
    #[error("Error during request encoding: {0}")]
    Request(
        #[from]
        #[source]
        EncodeError,
    ),
    /// Error parsing response
    #[error("Error during response parsing: {0}")]
    Response(
        #[from]
        #[source]
        DecodeError,
    ),
    /// Invalid request header
    #[error("{0}")]
    Http(
        #[from]
        #[source]
        HttpError,
    ),
    /// Http2 error
    #[error("Http2 error {0}")]
    H2(
        #[from]
        #[source]
        ntex_h2::OperationError,
    ),
    /// Response took too long
    #[error("Timeout while waiting for response")]
    Timeout,
    /// Other error, for example a request body or query serialization error
    #[error("{0}")]
    Error(
        #[from]
        #[source]
        Rc<dyn StdError>,
    ),
}

impl Clone for ClientError {
    fn clone(&self) -> ClientError {
        match self {
            ClientError::Url(err) => ClientError::Url(*err),
            ClientError::Connect(err) => ClientError::Connect(err.clone()),
            ClientError::Request(err) => ClientError::Request(err.clone()),
            ClientError::Response(err) => ClientError::Response(*err),
            ClientError::Http(err) => ClientError::Http(*err),
            ClientError::H2(err) => ClientError::H2(*err),
            ClientError::Timeout => ClientError::Timeout,
            ClientError::Error(err) => ClientError::Error(err.clone()),
            ClientError::Send(err) => ClientError::Send(crate::util::clone_io_error(err)),
        }
    }
}

impl From<Either<EncodeError, io::Error>> for ClientError {
    fn from(err: Either<EncodeError, io::Error>) -> Self {
        match err {
            Either::Left(err) => ClientError::Request(err),
            Either::Right(err) => ClientError::Send(err),
        }
    }
}

impl From<Either<DecodeError, io::Error>> for ClientError {
    fn from(err: Either<DecodeError, io::Error>) -> Self {
        match err {
            Either::Left(err) => ClientError::Response(err),
            Either::Right(err) => ClientError::Send(err),
        }
    }
}

impl ErrorDiagnostic for ClientError {
    fn signature(&self) -> &'static str {
        match self {
            ClientError::Url(_) => "ntex-client-Url",
            ClientError::Http(_) => "ntex-client-Http",
            ClientError::Connect(err) => err.signature(),
            ClientError::Send(err) => err.signature(),
            ClientError::Request(_) => "ntex-client-Request",
            ClientError::Response(_) => "ntex-client-Response",
            ClientError::Timeout => "ntex-client-Timeout",
            ClientError::Error(_) => "ntex-client-Error",
            ClientError::H2(err) => err.signature(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_error_from() {
        let err = ConnectError::from(crate::connect::ConnectError::InvalidInput);
        assert!(matches!(err, ConnectError::InvalidInput));
        assert_eq!(err.signature(), "ntex-client-connect-InvalidInput");
        assert!(matches!(err.clone(), ConnectError::InvalidInput));
    }

    #[test]
    fn connect_error_conversions() {
        use crate::connect::ConnectError as Base;

        let cases = [
            (
                Base::Resolver(io::Error::other("dns")),
                "ntex-client-connect-Resolver",
            ),
            (Base::NoRecords, "ntex-client-connect-NoRecords"),
            (Base::Unresolved, "ntex-client-connect-Unresolved"),
            (
                Base::Io(io::Error::other("io")),
                "ntex-client-connect-Disconnected",
            ),
        ];
        for (err, sig) in cases {
            let err = ConnectError::from(err);
            assert_eq!(err.signature(), sig);
            assert_eq!(err.clone().signature(), sig);
        }
        for err in [
            ConnectError::SslIsNotSupported,
            ConnectError::Disconnected(None),
        ] {
            assert_eq!(err.clone().to_string(), err.to_string());
            assert_eq!(err.clone().signature(), err.signature());
        }

        let err = ConnectError::Disconnected(Some(io::Error::other("gone")));
        let ConnectError::Disconnected(Some(e)) = err.clone() else {
            panic!()
        };
        assert_eq!(e.kind(), io::ErrorKind::Other);
        assert!(e.to_string().contains("gone"), "{e}");
    }

    #[test]
    fn client_error_clone_and_signature() {
        let errs = [
            (
                ClientError::from(InvalidUrl::MissingHost),
                "ntex-client-Url",
            ),
            (
                ClientError::from(ConnectError::NoRecords),
                "ntex-client-connect-NoRecords",
            ),
            (
                ClientError::from(Either::<EncodeError, io::Error>::Left(
                    EncodeError::UnexpectedEof,
                )),
                "ntex-client-Request",
            ),
            (
                ClientError::from(Either::<DecodeError, io::Error>::Left(DecodeError::Header)),
                "ntex-client-Response",
            ),
            (
                ClientError::from(HttpError::from(
                    crate::http::header::HeaderName::try_from("\n").unwrap_err(),
                )),
                "ntex-client-Http",
            ),
            (ClientError::Timeout, "ntex-client-Timeout"),
            (
                ClientError::from(Rc::new(io::Error::other("other")) as Rc<dyn StdError>),
                "ntex-client-Error",
            ),
            (
                ClientError::from(ntex_h2::OperationError::Disconnected),
                ntex_h2::OperationError::Disconnected.signature(),
            ),
        ];
        for (err, sig) in errs {
            assert_eq!(err.signature(), sig, "{err:?}");
            let cloned = err.clone();
            assert_eq!(cloned.signature(), sig);
            if !matches!(err, ClientError::Connect(_)) {
                assert_eq!(cloned.to_string(), err.to_string());
            }
        }

        let io_err = |msg| io::Error::new(io::ErrorKind::BrokenPipe, msg);
        for (err, msg) in [
            (
                ClientError::from(Either::<EncodeError, _>::Right(io_err("write"))),
                "write",
            ),
            (
                ClientError::from(Either::<DecodeError, _>::Right(io_err("read"))),
                "read",
            ),
        ] {
            let ClientError::Send(e) = err.clone() else {
                panic!("{err:?}")
            };
            // the cloned io error keeps its kind, the message is not preserved as is
            assert_eq!(e.kind(), io::ErrorKind::BrokenPipe);
            assert!(e.to_string().contains(msg), "{e}");
            assert_eq!(err.signature(), e.signature());
        }
    }

    #[test]
    fn payload_errors() {
        let err = ClientPayloadError::from(PayloadError::Overflow);
        assert_eq!(err.signature(), "ntex-client-Payload");
        assert!(matches!(*err, PayloadError::Overflow));

        let json_err = serde_json::from_str::<u32>("x").unwrap_err();
        let errs = [
            (JsonPayloadError::ContentType, "ntex-client-JsonContentType"),
            (
                JsonPayloadError::from(json_err),
                "ntex-client-JsonDeserialize",
            ),
            (
                JsonPayloadError::from(PayloadError::Overflow),
                "ntex-client-JsonPayload",
            ),
        ];
        for (err, sig) in errs {
            assert_eq!(err.signature(), sig);
            assert_eq!(err.clone().signature(), sig);
        }
        // the deserialize error is not cloneable
        let err = JsonPayloadError::from(serde_json::from_str::<u32>("x").unwrap_err());
        assert!(matches!(err.clone(), JsonPayloadError::Deserialize(None)));
        assert!(matches!(
            JsonPayloadError::from(PayloadError::Overflow).clone(),
            JsonPayloadError::Payload(ClientPayloadError(PayloadError::Overflow))
        ));
    }
}
