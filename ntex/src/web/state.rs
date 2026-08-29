use super::error::WebError;

pub trait AppState: 'static {
    type Error;
}

impl AppState for () {
    type Error = WebError;
}
