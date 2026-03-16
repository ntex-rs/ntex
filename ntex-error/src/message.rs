use std::{error::Error as StdError, fmt, rc::Rc};

use ntex_bytes::ByteString;

use crate::{ErrorDiagnostic, ResultKind, ResultType};

pub fn fmt_err_string(e: &dyn StdError) -> String {
    let mut buf = String::new();
    _ = fmt_err(&mut buf, e);
    buf
}

pub fn fmt_err(f: &mut dyn fmt::Write, e: &dyn StdError) -> fmt::Result {
    let mut current = Some(e);
    while let Some(std_err) = current {
        writeln!(f, "{std_err}")?;
        current = std_err.source();
    }
    Ok(())
}

pub fn fmt_diag_string<K: ResultKind>(e: &dyn ErrorDiagnostic<Kind = K>) -> String {
    let mut buf = String::new();
    _ = fmt_diag(&mut buf, e);
    buf
}

pub fn fmt_diag<K>(f: &mut dyn fmt::Write, e: &dyn ErrorDiagnostic<Kind = K>) -> fmt::Result
where
    K: ResultKind,
{
    let k = e.kind();
    let tp = k.tp();

    writeln!(f, "err: {e}")?;
    writeln!(f, "type: {}", tp.as_str())?;
    writeln!(f, "signature: {}", k.signature())?;

    if let Some(svc) = e.service() {
        writeln!(f, "service: {svc}")?;
    }

    writeln!(f, "\n{e:?}")?;

    let mut current = e.source();
    while let Some(err) = current {
        writeln!(f, "{err:?}")?;
        current = err.source();
    }

    if tp == ResultType::ServiceError
        && let Some(bt) = e.backtrace()
    {
        writeln!(f, "{bt}")?;
    }

    Ok(())
}

#[derive(Clone, PartialEq, Eq, thiserror::Error)]
pub struct ErrorMessage(ByteString);

#[derive(Clone)]
pub struct ErrorMessageChained {
    msg: ByteString,
    source: Option<Rc<dyn StdError>>,
}

impl ErrorMessageChained {
    pub fn new<M, E>(ctx: M, source: E) -> Self
    where
        M: Into<ErrorMessage>,
        E: StdError + 'static,
    {
        ErrorMessageChained {
            msg: ctx.into().into_string(),
            source: Some(Rc::new(source)),
        }
    }

    /// Construct `ErrorMessageChained` from `ByteString`
    pub const fn from_bstr(msg: ByteString) -> Self {
        Self { msg, source: None }
    }

    pub fn msg(&self) -> &ByteString {
        &self.msg
    }
}

impl ErrorMessage {
    /// Construct new empty `ErrorMessage`
    pub const fn empty() -> Self {
        Self(ByteString::from_static(""))
    }

    /// Construct `ErrorMessage` from `ByteString`
    pub const fn from_bstr(msg: ByteString) -> ErrorMessage {
        ErrorMessage(msg)
    }

    /// Construct `ErrorMessage` from static string
    pub const fn from_static(msg: &'static str) -> Self {
        ErrorMessage(ByteString::from_static(msg))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_bstr(&self) -> &ByteString {
        &self.0
    }

    pub fn into_string(self) -> ByteString {
        self.0
    }

    pub fn with_source<E: StdError + 'static>(self, source: E) -> ErrorMessageChained {
        ErrorMessageChained::new(self, source)
    }
}

impl From<String> for ErrorMessage {
    fn from(value: String) -> Self {
        Self(ByteString::from(value))
    }
}

impl From<ByteString> for ErrorMessage {
    fn from(value: ByteString) -> Self {
        Self(value)
    }
}

impl From<&'static str> for ErrorMessage {
    fn from(value: &'static str) -> Self {
        Self(ByteString::from_static(value))
    }
}

impl fmt::Debug for ErrorMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::Display for ErrorMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl From<ErrorMessage> for ByteString {
    fn from(msg: ErrorMessage) -> Self {
        msg.0
    }
}

impl<'a> From<&'a ErrorMessage> for ByteString {
    fn from(msg: &'a ErrorMessage) -> Self {
        msg.0.clone()
    }
}

impl<M: Into<ErrorMessage>> From<M> for ErrorMessageChained {
    fn from(value: M) -> Self {
        ErrorMessageChained {
            msg: value.into().0,
            source: None,
        }
    }
}

impl StdError for ErrorMessageChained {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source.as_ref().map(AsRef::as_ref)
    }
}

impl fmt::Debug for ErrorMessageChained {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self, f)
    }
}

impl fmt::Display for ErrorMessageChained {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.msg.is_empty() {
            Ok(())
        } else {
            fmt::Display::fmt(&self.msg, f)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    #[test]
    fn error_message() {
        let msg = ErrorMessage::empty();
        assert!(msg.is_empty());
        assert_eq!(msg.as_str(), "");
        assert_eq!(msg.as_bstr(), ByteString::new());
        assert_eq!(ByteString::new(), msg.as_bstr());
        assert_eq!(ByteString::new(), msg.into_string());

        let msg = ErrorMessage::from("test");
        assert!(!msg.is_empty());
        assert_eq!(format!("{msg}"), "test");
        assert_eq!(format!("{msg:?}"), "test");
        assert_eq!(msg.as_str(), "test");
        assert_eq!(msg.as_bstr(), ByteString::from("test"));

        let msg = ErrorMessage::from("test".to_string());
        assert!(!msg.is_empty());
        assert_eq!(msg.as_str(), "test");
        assert_eq!(msg.as_bstr(), ByteString::from("test"));

        let msg = ErrorMessage::from_bstr(ByteString::from("test"));
        assert!(!msg.is_empty());
        assert_eq!(msg.as_str(), "test");
        assert_eq!(msg.as_bstr(), ByteString::from("test"));

        let msg = ErrorMessage::from(ByteString::from("test"));
        assert!(!msg.is_empty());
        assert_eq!(msg.as_str(), "test");
        assert_eq!(msg.as_bstr(), ByteString::from("test"));

        let msg = ErrorMessage::from_static("test");
        assert!(!msg.is_empty());
        assert_eq!(msg.as_str(), "test");
        assert_eq!(msg.as_bstr(), ByteString::from("test"));

        assert_eq!(ByteString::from(&msg), "test");
        assert_eq!(ByteString::from(msg), "test");
    }

    #[test]
    fn error_message_chained() {
        let chained = ErrorMessageChained::from(ByteString::from("test"));
        assert_eq!(chained.msg(), "test");
        assert!(chained.source().is_none());

        let chained = ErrorMessageChained::from_bstr(ByteString::from("test"));
        assert_eq!(chained.msg(), "test");
        assert!(chained.source().is_none());
        assert_eq!(format!("{chained}"), "test");
        assert_eq!(format!("{chained:?}"), "test");

        let msg = ErrorMessage::from(ByteString::from("test"));
        let chained = msg.with_source(io::Error::other("io-test"));
        assert_eq!(chained.msg(), "test");
        assert!(chained.source().is_some());

        let err = ErrorMessageChained::new("test", io::Error::other("io-test"));
        let msg = fmt_err_string(&err);
        assert_eq!(msg, "test\nio-test\n");
    }
}
