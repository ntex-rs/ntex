use std::{error::Error, fmt, rc::Rc};

use ntex_bytes::ByteString;

pub fn fmt_err_string(e: &dyn Error) -> String {
    let mut buf = String::new();
    _ = fmt_err(&mut buf, e);
    buf
}

pub fn fmt_err(f: &mut dyn fmt::Write, e: &dyn Error) -> fmt::Result {
    let mut current = Some(e);
    while let Some(std_err) = current {
        writeln!(f, "{std_err}")?;
        current = std_err.source();
    }
    Ok(())
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub struct ErrorMessage(ByteString);

#[derive(Debug, Clone, thiserror::Error)]
#[error("{msg}")]
pub struct ErrorMessageChained {
    msg: ByteString,
    #[source]
    source: Option<Rc<dyn Error>>,
}

impl ErrorMessageChained {
    pub fn new<M, E>(ctx: M, source: E) -> Self
    where
        M: Into<ErrorMessage>,
        E: Error + 'static,
    {
        ErrorMessageChained {
            msg: ctx.into().into_string(),
            source: Some(Rc::new(source)),
        }
    }

    /// Construct ErrorMessage from ErrorMessageChained
    pub const fn from_bstr(msg: ByteString) -> Self {
        Self { msg, source: None }
    }

    pub fn msg(&self) -> &ByteString {
        &self.msg
    }

    pub fn source(&self) -> &Option<Rc<dyn Error>> {
        &self.source
    }
}

impl ErrorMessage {
    /// Construct new empty ErrorMessage
    pub const fn empty() -> Self {
        Self(ByteString::from_static(""))
    }

    /// Construct ErrorMessage from ByteString
    pub const fn from_bstr(msg: ByteString) -> ErrorMessage {
        ErrorMessage(msg)
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

    pub fn with_source<E: Error + 'static>(self, source: E) -> ErrorMessageChained {
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

impl fmt::Display for ErrorMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
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

#[cfg(test)]
mod tests {
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
        assert_eq!(msg.as_str(), "test");
        assert_eq!(msg.as_bstr(), ByteString::from("test"));

        let msg = ErrorMessage::from("test".to_string());
        assert!(!msg.is_empty());
        assert_eq!(msg.as_str(), "test");
        assert_eq!(msg.as_bstr(), ByteString::from("test"));

        let msg = ErrorMessage::from(ByteString::from("test"));
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
    }
}
