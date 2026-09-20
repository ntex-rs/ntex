use super::error::DefaultError;

pub trait State: 'static {
    type Error;
}

impl State for () {
    type Error = DefaultError;
}

#[derive(Clone, Default)]
pub struct AppState<T> {
    state: T,
}

impl<T> AppState<T> {
    pub fn new(state: T) -> Self {
        AppState { state }
    }

    pub fn st(&self) -> &T {
        &self.state
    }
}

impl<T: 'static> State for AppState<T> {
    type Error = DefaultError;
}

impl<T> std::ops::Deref for AppState<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.state
    }
}
