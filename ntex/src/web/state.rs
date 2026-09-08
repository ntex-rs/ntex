use super::error::DefaultError;

pub trait AppState: 'static {
    type Error;
}

impl AppState for () {
    type Error = DefaultError;
}
