use std::rc::Rc;

use crate::router::{IntoPattern, ResourceDef};
use crate::service::cfg::{Cfg, SharedCfg};
use crate::service::{IntoServiceFactory, ServiceFactory, boxed};
use crate::util::Extensions;

use super::config::WebAppConfig;
use super::dev::insert_slash;
use super::error::ErrorRenderer;
use super::guard::{AllGuard, Guard};
use super::{request::WebRequest, response::WebResponse, rmap::ResourceMap};

pub trait WebServiceFactory<Err: ErrorRenderer> {
    fn register(self, config: &mut WebServiceConfig<Err>);
}

pub(super) trait AppServiceFactory<Err: ErrorRenderer> {
    fn register(&mut self, config: &mut WebServiceConfig<Err>);
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

impl<T, Err> AppServiceFactory<Err> for ServiceFactoryWrapper<T>
where
    T: WebServiceFactory<Err>,
    Err: ErrorRenderer,
{
    fn register(&mut self, config: &mut WebServiceConfig<Err>) {
        if let Some(item) = self.factory.take() {
            item.register(config);
        }
    }
}

type Guards = Vec<Box<dyn Guard>>;
type HttpServiceFactory<Err: ErrorRenderer> =
    boxed::BoxServiceFactory<SharedCfg, WebRequest<Err>, WebResponse, Err::Container, ()>;

#[derive(Debug, Clone)]
pub(crate) struct AppState(Rc<AppStateInner>);

#[derive(Debug)]
struct AppStateInner {
    ext: Extensions,
    parent: Option<AppState>,
    config: Cfg<WebAppConfig>,
}

impl AppState {
    pub(crate) fn new(
        ext: Extensions,
        parent: Option<AppState>,
        config: Cfg<WebAppConfig>,
    ) -> Self {
        AppState(Rc::new(AppStateInner {
            ext,
            parent,
            config,
        }))
    }

    pub(crate) fn config(&self) -> Cfg<WebAppConfig> {
        self.0.config
    }

    pub(crate) fn get<T: 'static>(&self) -> Option<&T> {
        let result = self.0.ext.get::<T>();
        if result.is_some() {
            result
        } else if let Some(parent) = self.0.parent.as_ref() {
            parent.get::<T>()
        } else {
            None
        }
    }

    pub(crate) fn contains<T: 'static>(&self) -> bool {
        if self.0.ext.contains::<T>() {
            true
        } else if let Some(parent) = self.0.parent.as_ref() {
            parent.contains::<T>()
        } else {
            false
        }
    }
}

/// Application service configuration
pub struct WebServiceConfig<Err: ErrorRenderer> {
    state: AppState,
    root: bool,
    default: Rc<HttpServiceFactory<Err>>,
    services: Vec<(
        ResourceDef,
        HttpServiceFactory<Err>,
        Option<Guards>,
        Option<Rc<ResourceMap>>,
    )>,
}

impl<Err: ErrorRenderer> WebServiceConfig<Err> {
    /// Crate server settings instance
    pub(crate) fn new(state: AppState, default: Rc<HttpServiceFactory<Err>>) -> Self {
        WebServiceConfig {
            state,
            default,
            root: true,
            services: Vec::new(),
        }
    }

    /// Check if root is beeing configured
    pub fn is_root(&self) -> bool {
        self.root
    }

    pub(super) fn state(&self) -> &AppState {
        &self.state
    }

    pub(crate) fn into_services(
        self,
    ) -> Vec<(
        ResourceDef,
        HttpServiceFactory<Err>,
        Option<Guards>,
        Option<Rc<ResourceMap>>,
    )> {
        self.services
    }

    pub(crate) fn clone_config(&self, state: Option<AppState>) -> Self {
        WebServiceConfig {
            state: state.unwrap_or_else(|| self.state.clone()),
            default: self.default.clone(),
            services: Vec::new(),
            root: false,
        }
    }

    /// Service configuration
    pub fn config(&self) -> Cfg<WebAppConfig> {
        self.state.config()
    }

    /// Default resource
    pub fn default_service(&self) -> Rc<HttpServiceFactory<Err>> {
        self.default.clone()
    }

    /// Register http service
    pub fn register_service<F, S>(
        &mut self,
        rdef: ResourceDef,
        guards: Option<Vec<Box<dyn Guard>>>,
        factory: F,
        nested: Option<Rc<ResourceMap>>,
    ) where
        F: IntoServiceFactory<S, WebRequest<Err>, SharedCfg>,
        S: ServiceFactory<
                WebRequest<Err>,
                SharedCfg,
                Response = WebResponse,
                Error = Err::Container,
                InitError = (),
            > + 'static,
    {
        self.services
            .push((rdef, boxed::factory(factory.into_factory()), guards, nested));
    }
}

/// Create service adapter for a specific path.
///
/// ```rust
/// use ntex::web::{self, guard, App, HttpResponse, Error, DefaultError};
///
/// async fn my_service(req: web::WebRequest<DefaultError>) -> Result<web::WebResponse, Error> {
///     Ok(req.into_response(HttpResponse::Ok().finish()))
/// }
///
/// let app = App::new().service(
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
    /// use ntex::web::{self, guard, App, DefaultError, Error, HttpResponse};
    ///
    /// async fn index(req: web::WebRequest<DefaultError>) -> Result<web::WebResponse, Error> {
    ///     Ok(req.into_response(HttpResponse::Ok().finish()))
    /// }
    ///
    /// fn main() {
    ///     let app = App::new()
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
    pub fn finish<T, F, Err>(self, service: F) -> impl WebServiceFactory<Err>
    where
        F: IntoServiceFactory<T, WebRequest<Err>, SharedCfg>,
        T: ServiceFactory<
                WebRequest<Err>,
                SharedCfg,
                Response = WebResponse,
                Error = Err::Container,
            > + 'static,
        Err: ErrorRenderer,
    {
        WebServiceImpl {
            srv: service.into_factory().map_init_err(|_| ()),
            rdef: self.rdef,
            name: self.name,
            guards: self.guards,
        }
    }
}

struct WebServiceImpl<T> {
    srv: T,
    rdef: Vec<String>,
    name: Option<String>,
    guards: AllGuard,
}

impl<T, Err> WebServiceFactory<Err> for WebServiceImpl<T>
where
    T: ServiceFactory<
            WebRequest<Err>,
            SharedCfg,
            Response = WebResponse,
            Error = Err::Container,
            InitError = (),
        > + 'static,
    Err: ErrorRenderer,
{
    fn register(mut self, config: &mut WebServiceConfig<Err>) {
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
        config.register_service(rdef, guards, self.srv, None);
    }
}

#[allow(unused_parens)]
impl<T, Err> WebServiceFactory<Err> for Vec<T>
where
    Err: ErrorRenderer,
    T: WebServiceFactory<Err> + 'static,
{
    fn register(mut self, config: &mut WebServiceConfig<Err>) {
        for service in self.drain(..) {
            service.register(config);
        }
    }
}

macro_rules! tuple_web_service(
    {$(#[$meta:meta])* $(($n:tt, $T:ident)),+} => {

        $(#[$meta])*
        impl<Err: ErrorRenderer, $($T: WebServiceFactory<Err> + 'static),+> WebServiceFactory<Err> for ($($T,)+) {
            fn register(self, config: &mut WebServiceConfig<Err>) {
                $(
                    self.$n.register(config);
                )+
            }
        }
    }
);

impl<Err, T, const N: usize> WebServiceFactory<Err> for [T; N]
where
    Err: ErrorRenderer,
    T: WebServiceFactory<Err> + 'static,
{
    fn register(self, config: &mut WebServiceConfig<Err>) {
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
    use crate::web::{self, App, DefaultError, HttpResponse, guard};

    #[test]
    fn test_service_request() {
        let req = TestRequest::default().to_srv_request();
        let (r, pl) = req.into_parts();
        assert!(WebRequest::<DefaultError>::from_parts(r, pl).is_ok());

        let req = TestRequest::default().to_srv_request();
        let (r, pl) = req.into_parts();
        let _r2 = r.clone();
        assert!(WebRequest::<DefaultError>::from_parts(r, pl).is_err());

        let req = TestRequest::default().to_srv_request();
        let (r, _pl) = req.into_parts();
        assert!(WebRequest::<DefaultError>::from_request(r).is_ok());

        let req = TestRequest::default().to_srv_request();
        let (r, _pl) = req.into_parts();
        let _r2 = r.clone();
        assert!(WebRequest::<DefaultError>::from_request(r).is_err());
    }

    #[crate::rt_test]
    async fn test_service() {
        let srv = init_service(App::new().service(
            web::service("/test").name("test").finish(
                |req: WebRequest<DefaultError>| async move {
                    Ok(req.into_response(HttpResponse::Ok().finish()))
                },
            ),
        ))
        .await;
        let req = TestRequest::with_uri("/test").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let srv = init_service(App::new().service(
            web::service("/test").guard(guard::Get()).finish(
                |req: WebRequest<DefaultError>| async move {
                    Ok(req.into_response(HttpResponse::Ok().finish()))
                },
            ),
        ))
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
            web::resource("/test1").to(|| async { HttpResponse::Ok() }),
            web::resource("/test2").to(|| async { HttpResponse::Ok() }),
        ]))
        .await;
        let req = TestRequest::with_uri("/test1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let req = TestRequest::with_uri("/test2").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let srv = init_service(App::new().service((
            web::resource("/test1").to(|| async { HttpResponse::Ok() }),
            web::resource("/test2").to(|| async { HttpResponse::Ok() }),
        )))
        .await;
        let req = TestRequest::with_uri("/test1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let req = TestRequest::with_uri("/test2").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let srv = init_service(App::new().service(vec![
            web::resource("/test1").to(|| async { HttpResponse::Ok() }),
            web::resource("/test2").to(|| async { HttpResponse::Ok() }),
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
