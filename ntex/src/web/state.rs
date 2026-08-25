use super::error::{ErrorContainer, WebError};

pub trait AppState: 'static {
    type Error: ErrorContainer;
}

impl AppState for () {
    type Error = WebError;
}
