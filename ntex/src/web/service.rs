use std::rc::Rc;

use crate::router::{IntoPattern, ResourceDef};
use crate::service::{IntoServiceFactory, ServiceFactory, boxed};

use super::guard::{AllGuard, Guard};
use super::{AppState, HttpService, WebRequest, WebResponse, dev::insert_slash, rmap::ResourceMap};

pub trait WebServiceFactory<St: AppState> {
    fn register(self, config: &mut WebServiceConfig<St>);
}

pub(super) trait AppServiceFactory<St: AppState> {
    fn register(&mut self, config: &mut WebServiceConfig<St>);
}

pub(super) struct ServiceFactoryWrapper<T> {
    factory: Option<T>,
}

impl<T> ServiceFactoryWrapper<T> {
    pub(super) fn new(factory: T) -> Self {
        Self {
            factory: Some(factory),
        }
    }
}

impl<T, St> AppServiceFactory<St> for ServiceFactoryWrapper<T>
where
    T: WebServiceFactory<St>,
    St: AppState,
{
    fn register(&mut self, config: &mut WebServiceConfig<St>) {
        if let Some(item) = self.factory.take() {
            item.register(config);
        }
    }
}

type Guards = Vec<Box<dyn Guard>>;

/// Application service configuration
#[derive(derive_more::Debug)]
#[debug("WebServiceConfig")]
pub struct WebServiceConfig<St: AppState> {
    root: bool,
    default: HttpService<St>,
    services: Vec<(
        ResourceDef,
        HttpService<St>,
        Option<Guards>,
        Option<Rc<ResourceMap>>,
    )>,
}

impl<St: AppState> WebServiceConfig<St> {
    /// Crate server settings instance
    pub(crate) fn new(default: HttpService<St>) -> Self {
        WebServiceConfig {
            default,
            root: true,
            services: Vec::new(),
        }
    }

    /// Check if root is beeing configured
    pub fn is_root(&self) -> bool {
        self.root
    }

    pub(crate) fn into_services(
        self,
    ) -> (
        Vec<(
            ResourceDef,
            HttpService<St>,
            Option<Guards>,
            Option<Rc<ResourceMap>>,
        )>,
        HttpService<St>,
    ) {
        (self.services, self.default)
    }

    pub(crate) fn get_nested(&self) -> Self {
        WebServiceConfig {
            default: self.default.clone(),
            services: Vec::new(),
            root: false,
        }
    }

    /// Default resource
    pub fn default_service(&self) -> HttpService<St> {
        self.default.clone()
    }

    /// Register http service
    pub fn register_service<S>(
        &mut self,
        rdef: ResourceDef,
        factory: impl IntoServiceFactory<S, St, WebRequest>,
        guards: Option<Vec<Box<dyn Guard>>>,
        nested: Option<Rc<ResourceMap>>,
    ) where
        S: ServiceFactory<St, WebRequest, Res = WebResponse, Error = St::Error, InitError = ()>
            + 'static,
    {
        self.services
            .push((rdef, boxed::factory(factory.into_factory()), guards, nested));
    }
}

/// Create service adapter for a specific path.
///
/// ```rust
/// use ntex::web::{self, guard, App, HttpResponse, WebError};
///
/// async fn my_service(req: web::WebRequest) -> Result<web::WebResponse, WebError> {
///     Ok(req.into_response(HttpResponse::Ok().finish()))
/// }
///
/// let app = App::default().service(
///     web::service("/users/*")
///         .guard(guard::Header("content-type", "text/plain"))
///         .finish(my_service)
/// );
/// ```
#[derive(Debug)]
pub struct WebServiceAdapter {
    rdef: Vec<String>,
    name: Option<String>,
    guards: AllGuard,
}

impl WebServiceAdapter {
    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    /// Create new `WebServiceAdapter` instance.
    pub fn new<T: IntoPattern>(path: T) -> Self {
        WebServiceAdapter {
            rdef: path.patterns(),
            name: None,
            guards: AllGuard::default(),
        }
    }

    /// Set service name.
    ///
    /// Name is used for url generation.
    #[must_use]
    pub fn name(mut self, name: &str) -> Self {
        self.name = Some(name.to_string());
        self
    }

    /// Add match guard to a web service.
    ///
    /// ```rust
    /// use ntex::web::{self, guard, App, WebError, HttpResponse};
    ///
    /// async fn index(req: web::WebRequest) -> Result<web::WebResponse, WebError> {
    ///     Ok(req.into_response(HttpResponse::Ok().finish()))
    /// }
    ///
    /// fn main() {
    ///     let app = App::default()
    ///         .service(
    ///             web::service("/app")
    ///                 .guard(guard::Header("content-type", "text/plain"))
    ///                 .finish(index)
    ///         );
    /// }
    /// ```
    #[must_use]
    pub fn guard<G: Guard + 'static>(mut self, guard: G) -> Self {
        self.guards.add(guard);
        self
    }

    /// Set a service factory implementation and generate web service.
    pub fn finish<St, T, F>(self, service: F) -> impl WebServiceFactory<St>
    where
        St: AppState,
        F: IntoServiceFactory<T, St, WebRequest>,
        T: ServiceFactory<St, WebRequest, Res = WebResponse, Error = St::Error> + 'static,
    {
        WebServiceImpl {
            srv: service.into_factory().map_init_err(|_| ()),
            rdef: self.rdef,
            name: self.name,
            guards: self.guards,
        }
    }
}

struct WebServiceImpl<Sf> {
    srv: Sf,
    rdef: Vec<String>,
    name: Option<String>,
    guards: AllGuard,
}

impl<Sf, St> WebServiceFactory<St> for WebServiceImpl<Sf>
where
    St: AppState,
    Sf: ServiceFactory<St, WebRequest, Res = WebResponse, Error = St::Error, InitError = ()>
        + 'static,
{
    fn register(mut self, config: &mut WebServiceConfig<St>) {
        let guards = if self.guards.0.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.guards.0))
        };

        let mut rdef = if config.is_root() || !self.rdef.is_empty() {
            ResourceDef::new(insert_slash(self.rdef))
        } else {
            ResourceDef::new(self.rdef)
        };
        if let Some(ref name) = self.name {
            rdef.name_mut().clone_from(name);
        }
        config.register_service(rdef, self.srv, guards, None);
    }
}

#[allow(unused_parens)]
impl<T, St> WebServiceFactory<St> for Vec<T>
where
    T: WebServiceFactory<St> + 'static,
    St: AppState,
{
    fn register(mut self, config: &mut WebServiceConfig<St>) {
        for service in self.drain(..) {
            service.register(config);
        }
    }
}

macro_rules! tuple_web_service(
    {$(#[$meta:meta])* $(($n:tt, $T:ident)),+} => {

        $(#[$meta])*
        impl<St: AppState, $($T: WebServiceFactory<St> + 'static),+> WebServiceFactory<St> for ($($T,)+) {
            fn register(self, config: &mut WebServiceConfig<St>) {
                $(
                    self.$n.register(config);
                )+
            }
        }
    }
);

impl<St, T, const N: usize> WebServiceFactory<St> for [T; N]
where
    St: AppState,
    T: WebServiceFactory<St> + 'static,
{
    fn register(self, config: &mut WebServiceConfig<St>) {
        for t in self {
            t.register(config);
        }
    }
}

#[allow(non_snake_case, clippy::wildcard_imports)]
#[rustfmt::skip]
mod m {
    use super::*;
    use variadics_please::all_tuples_enumerated;

    all_tuples_enumerated!(#[doc(fake_variadic)] tuple_web_service, 1, 24, T);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Method, StatusCode};
    use crate::web::test::{TestRequest, init_service};
    use crate::web::{self, App, HttpResponse, guard};

    #[crate::rt_test]
    async fn test_service() {
        let srv = init_service(
            App::new().service(web::service("/test").name("test").finish(
                async move |req: WebRequest| Ok(req.into_response(HttpResponse::Ok().finish())),
            )),
        )
        .await;
        let req = TestRequest::with_uri("/test").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let srv = init_service(
            App::new().service(web::service("/test").guard(guard::Get()).finish(
                async move |req: WebRequest| Ok(req.into_response(HttpResponse::Ok().finish())),
            )),
        )
        .await;
        let req = TestRequest::with_uri("/test")
            .method(Method::PUT)
            .to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[crate::rt_test]
    async fn test_multi() {
        let srv = init_service(App::new().service([
            web::resource("/test1").to(async || HttpResponse::Ok()),
            web::resource("/test2").to(async || HttpResponse::Ok()),
        ]))
        .await;
        let req = TestRequest::with_uri("/test1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let req = TestRequest::with_uri("/test2").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let srv = init_service(App::new().service((
            web::resource("/test1").to(async || HttpResponse::Ok()),
            web::resource("/test2").to(async || HttpResponse::Ok()),
        )))
        .await;
        let req = TestRequest::with_uri("/test1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let req = TestRequest::with_uri("/test2").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let srv = init_service(App::new().service(vec![
            web::resource("/test1").to(async || HttpResponse::Ok()),
            web::resource("/test2").to(async || HttpResponse::Ok()),
        ]))
        .await;
        let req = TestRequest::with_uri("/test1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let req = TestRequest::with_uri("/test2").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn test_fmt_debug() {
        let req = TestRequest::get()
            .uri("/index.html?test=1")
            .header("x-test", "111")
            .to_srv_request();
        let s = format!("{req:?}");
        assert!(s.contains("WebRequest"));
        assert!(s.contains("test=1"));
        assert!(s.contains("x-test"));

        let res = HttpResponse::Ok().header("x-test", "111").finish();
        let res = TestRequest::post()
            .uri("/index.html?test=1")
            .to_srv_response(res);

        let s = format!("{res:?}");
        assert!(s.contains("WebResponse"));
        assert!(s.contains("x-test"));
    }
}
