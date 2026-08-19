use std::rc::Rc;

use crate::router::{IntoPattern, ResourceDef};
use crate::service::{IntoServiceFactory, ServiceFactory, boxed, cfg::SharedCfg};

use super::dev::insert_slash;
use super::error::ErrorRenderer;
use super::guard::{AllGuard, Guard};
use super::{HttpService, request::WebRequest, response::WebResponse, rmap::ResourceMap};

pub trait WebServiceFactory<St, Err: ErrorRenderer> {
    fn register(self, config: &mut WebServiceConfig<St, Err>);
}

pub(super) trait AppServiceFactory<St, Err: ErrorRenderer> {
    fn register(&mut self, config: &mut WebServiceConfig<St, Err>);
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

impl<T, St, Err> AppServiceFactory<St, Err> for ServiceFactoryWrapper<T>
where
    T: WebServiceFactory<St, Err>,
    Err: ErrorRenderer,
{
    fn register(&mut self, config: &mut WebServiceConfig<St, Err>) {
        if let Some(item) = self.factory.take() {
            item.register(config);
        }
    }
}

type Guards = Vec<Box<dyn Guard>>;

/// Application service configuration
#[derive(derive_more::Debug)]
#[debug("WebServiceConfig")]
pub struct WebServiceConfig<St, Err: ErrorRenderer> {
    root: bool,
    default: Rc<HttpService<St, Err>>,
    services: Vec<(
        ResourceDef,
        HttpService<St, Err>,
        Option<Guards>,
        Option<Rc<ResourceMap>>,
    )>,
}

impl<St: 'static, Err: ErrorRenderer> WebServiceConfig<St, Err> {
    /// Crate server settings instance
    pub(crate) fn new(default: Rc<HttpService<St, Err>>) -> Self {
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
            HttpService<St, Err>,
            Option<Guards>,
            Option<Rc<ResourceMap>>,
        )>,
        Rc<HttpService<St, Err>>,
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
    pub fn default_service(&self) -> Rc<HttpService<St, Err>> {
        self.default.clone()
    }

    /// Register http service
    pub fn register_service<S>(
        &mut self,
        rdef: ResourceDef,
        factory: impl IntoServiceFactory<S, St, WebRequest<Err>, SharedCfg>,
        guards: Option<Vec<Box<dyn Guard>>>,
        nested: Option<Rc<ResourceMap>>,
    ) where
        S: ServiceFactory<
                St,
                WebRequest<Err>,
                SharedCfg,
                Res = WebResponse,
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
    pub fn finish<St, T, F, Err>(self, service: F) -> impl WebServiceFactory<St, Err>
    where
        St: 'static,
        F: IntoServiceFactory<T, St, WebRequest<Err>, SharedCfg>,
        T: ServiceFactory<
                St,
                WebRequest<Err>,
                SharedCfg,
                Res = WebResponse,
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

struct WebServiceImpl<Sf> {
    srv: Sf,
    rdef: Vec<String>,
    name: Option<String>,
    guards: AllGuard,
}

impl<Sf, St, Err> WebServiceFactory<St, Err> for WebServiceImpl<Sf>
where
    St: 'static,
    Sf: ServiceFactory<
            St,
            WebRequest<Err>,
            SharedCfg,
            Res = WebResponse,
            Error = Err::Container,
            InitError = (),
        > + 'static,
    Err: ErrorRenderer,
{
    fn register(mut self, config: &mut WebServiceConfig<St, Err>) {
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
impl<T, St, Err> WebServiceFactory<St, Err> for Vec<T>
where
    Err: ErrorRenderer,
    T: WebServiceFactory<St, Err> + 'static,
{
    fn register(mut self, config: &mut WebServiceConfig<St, Err>) {
        for service in self.drain(..) {
            service.register(config);
        }
    }
}

macro_rules! tuple_web_service(
    {$(#[$meta:meta])* $(($n:tt, $T:ident)),+} => {

        $(#[$meta])*
        impl<St, Err: ErrorRenderer, $($T: WebServiceFactory<St, Err> + 'static),+> WebServiceFactory<St, Err> for ($($T,)+) {
            fn register(self, config: &mut WebServiceConfig<St, Err>) {
                $(
                    self.$n.register(config);
                )+
            }
        }
    }
);

impl<St, Err, T, const N: usize> WebServiceFactory<St, Err> for [T; N]
where
    Err: ErrorRenderer,
    T: WebServiceFactory<St, Err> + 'static,
{
    fn register(self, config: &mut WebServiceConfig<St, Err>) {
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
        let srv = init_service(
            App::new().service(web::service("/test").name("test").finish(
                async move |req: WebRequest<DefaultError>| {
                    Ok(req.into_response(HttpResponse::Ok().finish()))
                },
            )),
        )
        .await;
        let req = TestRequest::with_uri("/test").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let srv = init_service(App::new().service(
            web::service("/test").guard(guard::Get()).finish(
                async move |req: WebRequest<DefaultError>| {
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
