use super::error::DefaultError;

/// Application state used by web services.
///
/// A [`web::App`](super::App) is parameterized by a type that implements this
/// trait. The state is available to handlers registered with
/// [`Route::to_with_state()`](super::Route::to_with_state), and to filters,
/// middleware, and services through their service context.
///
/// The associated `Error` type selects the application's error domain, which
/// is used by [`WebError`](super::WebError) and
/// [`WebResponseError`](super::WebResponseError). Most applications use
/// [`DefaultError`].
///
/// The trait does not require `Clone`, but serving an application does:
/// the server and [`HttpService`](crate::http::HttpService) clone the state
/// for each connection.
///
/// ```rust
/// use ntex::web;
///
/// #[derive(Clone)]
/// struct ApplicationState {
///     greeting: String,
/// }
///
/// impl web::State for ApplicationState {
///     type Error = web::DefaultError;
/// }
/// ```
pub trait State: 'static {
    /// Error domain used by the application.
    type Error;
}

impl State for () {
    type Error = DefaultError;
}

/// Wraps a value so it can be used as application state with [`DefaultError`].
///
/// `AppState<T>` implements [`State`] for any `'static` `T`, so a separate
/// `State` implementation is not needed. It dereferences to the wrapped value
/// and implements `Clone` and `Default` when `T` does.
///
/// ```rust
/// use ntex::web;
///
/// #[derive(Clone)]
/// struct Settings {
///     service_name: &'static str,
/// }
///
/// let state = web::AppState::new(Settings { service_name: "users" });
/// assert_eq!(state.service_name, "users");
/// ```
#[derive(Clone, Default)]
pub struct AppState<T> {
    state: T,
}

impl<T> AppState<T> {
    /// Creates application state that wraps `state`.
    pub fn new(state: T) -> Self {
        AppState { state }
    }

    /// Returns a reference to the wrapped value.
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
