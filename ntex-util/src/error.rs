use std::{error::Error, fmt, rc::Rc};

use ntex_bytes::ByteString;

pub fn fmt_err_string(e: &(dyn Error + 'static)) -> String {
    let mut buf = String::new();
    _ = fmt_err(&mut buf, e);
    buf
}

pub fn fmt_err(f: &mut dyn fmt::Write, e: &(dyn Error + 'static)) -> fmt::Result {
    let mut current = Some(e as &(dyn Error + 'static));
    while let Some(std_err) = current {
        writeln!(f, "{}", std_err)?;
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

    pub fn msg(&self) -> &ByteString {
        &self.msg
    }

    pub fn source(&self) -> &Option<Rc<dyn Error>> {
        &self.source
    }
}

impl ErrorMessage {
    pub const fn empty() -> Self {
        Self(ByteString::from_static(""))
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
