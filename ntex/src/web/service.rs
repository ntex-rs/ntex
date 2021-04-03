use std::rc::Rc;

use crate::router::{IntoPattern, ResourceDef};
use crate::service::{boxed, IntoServiceFactory, ServiceFactory};
use crate::util::Extensions;

use super::config::AppConfig;
use super::dev::insert_slesh;
use super::error::ErrorRenderer;
use super::guard::Guard;
use super::request::WebRequest;
use super::response::WebResponse;
use super::rmap::ResourceMap;
use super::types::data::DataFactory;

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
            item.register(config)
        }
    }
}

type Guards = Vec<Box<dyn Guard>>;
type HttpServiceFactory<Err: ErrorRenderer> =
    boxed::BoxServiceFactory<(), WebRequest<Err>, WebResponse, Err::Container, ()>;

/// Application service configuration
pub struct WebServiceConfig<Err: ErrorRenderer> {
    config: AppConfig,
    root: bool,
    default: Rc<HttpServiceFactory<Err>>,
    services: Vec<(
        ResourceDef,
        HttpServiceFactory<Err>,
        Option<Guards>,
        Option<Rc<ResourceMap>>,
    )>,
    service_data: Rc<Vec<Box<dyn DataFactory>>>,
}

impl<Err: ErrorRenderer> WebServiceConfig<Err> {
    /// Crate server settings instance
    pub(crate) fn new(
        config: AppConfig,
        default: Rc<HttpServiceFactory<Err>>,
        service_data: Rc<Vec<Box<dyn DataFactory>>>,
    ) -> Self {
        WebServiceConfig {
            config,
            default,
            service_data,
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
        AppConfig,
        Vec<(
            ResourceDef,
            HttpServiceFactory<Err>,
            Option<Guards>,
            Option<Rc<ResourceMap>>,
        )>,
    ) {
        (self.config, self.services)
    }

    pub(crate) fn clone_config(&self) -> Self {
        WebServiceConfig {
            config: self.config.clone(),
            default: self.default.clone(),
            services: Vec::new(),
            root: false,
            service_data: self.service_data.clone(),
        }
    }

    /// Service configuration
    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    /// Default resource
    pub fn default_service(&self) -> Rc<HttpServiceFactory<Err>> {
        self.default.clone()
    }

    /// Set global route data
    pub fn set_service_data(&self, extensions: &mut Extensions) -> bool {
        for f in self.service_data.iter() {
            f.create(extensions);
        }
        !self.service_data.is_empty()
    }

    /// Register http service
    pub fn register_service<F, S>(
        &mut self,
        rdef: ResourceDef,
        guards: Option<Vec<Box<dyn Guard>>>,
        factory: F,
        nested: Option<Rc<ResourceMap>>,
    ) where
        F: IntoServiceFactory<S>,
        S: ServiceFactory<
                Config = (),
                Request = WebRequest<Err>,
                Response = WebResponse,
                Error = Err::Container,
                InitError = (),
            > + 'static,
    {
        self.services.push((
            rdef,
            boxed::factory(factory.into_factory()),
            guards,
            nested,
        ));
    }
}

/// Create service adapter for a specific path.
///
/// ```rust
/// use ntex::web::{self, dev, guard, App, HttpResponse, Error, DefaultError};
///
/// async fn my_service(req: dev::WebRequest<DefaultError>) -> Result<dev::WebResponse, Error> {
///     Ok(req.into_response(HttpResponse::Ok().finish()))
/// }
///
/// let app = App::new().service(
///     web::service("/users/*")
///         .guard(guard::Header("content-type", "text/plain"))
///         .finish(my_service)
/// );
/// ```
pub struct WebServiceAdapter {
    rdef: Vec<String>,
    name: Option<String>,
    guards: Vec<Box<dyn Guard>>,
}

impl WebServiceAdapter {
    /// Create new `WebServiceAdapter` instance.
    pub fn new<T: IntoPattern>(path: T) -> Self {
        WebServiceAdapter {
            rdef: path.patterns(),
            name: None,
            guards: Vec::new(),
        }
    }

    /// Set service name.
    ///
    /// Name is used for url generation.
    pub fn name(mut self, name: &str) -> Self {
        self.name = Some(name.to_string());
        self
    }

    /// Add match guard to a web service.
    ///
    /// ```rust
    /// use ntex::web::{self, guard, dev, App, DefaultError, Error, HttpResponse};
    ///
    /// async fn index(req: dev::WebRequest<DefaultError>) -> Result<dev::WebResponse, Error> {
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
    pub fn guard<G: Guard + 'static>(mut self, guard: G) -> Self {
        self.guards.push(Box::new(guard));
        self
    }

    /// Set a service factory implementation and generate web service.
    pub fn finish<T, F, Err>(self, service: F) -> impl WebServiceFactory<Err>
    where
        F: IntoServiceFactory<T>,
        T: ServiceFactory<
                Config = (),
                Request = WebRequest<Err>,
                Response = WebResponse,
                Error = Err::Container,
                InitError = (),
            > + 'static,
        Err: ErrorRenderer,
    {
        WebServiceImpl {
            srv: service.into_factory(),
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
    guards: Vec<Box<dyn Guard>>,
}

impl<T, Err> WebServiceFactory<Err> for WebServiceImpl<T>
where
    T: ServiceFactory<
            Config = (),
            Request = WebRequest<Err>,
            Response = WebResponse,
            Error = Err::Container,
            InitError = (),
        > + 'static,
    Err: ErrorRenderer,
{
    fn register(mut self, config: &mut WebServiceConfig<Err>) {
        let guards = if self.guards.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.guards))
        };

        let mut rdef = if config.is_root() || !self.rdef.is_empty() {
            ResourceDef::new(insert_slesh(self.rdef))
        } else {
            ResourceDef::new(self.rdef)
        };
        if let Some(ref name) = self.name {
            *rdef.name_mut() = name.clone();
        }
        config.register_service(rdef, guards, self.srv, None)
    }
}

/// WebServiceFactory implementation for a Vec<T>
#[allow(unused_parens)]
impl<Err, T> WebServiceFactory<Err> for Vec<T>
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

macro_rules! tuple_web_service({$(($n:tt, $T:ident)),+} => {
    /// WebServiceFactory implementation for a tuple
    #[allow(unused_parens)]
    impl<Err: ErrorRenderer, $($T: WebServiceFactory<Err> + 'static),+> WebServiceFactory<Err> for ($($T,)+) {
        fn register(self, config: &mut WebServiceConfig<Err>) {
            $(
                self.$n.register(config);
            )+
        }
    }
});

macro_rules! array_web_service({$num:tt, $($T:ident),+} => {
    /// WebServiceFactory implementation for an array
    #[allow(unused_parens)]
    impl<Err, T> WebServiceFactory<Err> for [T; $num]
    where
        Err: ErrorRenderer,
        T: WebServiceFactory<Err> + 'static,
    {
        fn register(self, config: &mut WebServiceConfig<Err>) {
            let [$($T,)+] = self;

            $(
                $T.register(config);
            )+
        }
    }
});

#[allow(non_snake_case)]
#[rustfmt::skip]
mod m {
    use super::*;

array_web_service!(1,A);
array_web_service!(2,A,B);
array_web_service!(3,A,B,C);
array_web_service!(4,A,B,C,D);
array_web_service!(5,A,B,C,D,E);
array_web_service!(6,A,B,C,D,E,F);
array_web_service!(7,A,B,C,D,E,F,G);
array_web_service!(8,A,B,C,D,E,F,G,H);
array_web_service!(9,A,B,C,D,E,F,G,H,I);
array_web_service!(10,A,B,C,D,E,F,G,H,I,J);
array_web_service!(11,A,B,C,D,E,F,G,H,I,J,K);
array_web_service!(12,A,B,C,D,E,F,G,H,I,J,K,L);
array_web_service!(13,A,B,C,D,E,F,G,H,I,J,K,L,M);
array_web_service!(14,A,B,C,D,E,F,G,H,I,J,K,L,M,N);
array_web_service!(15,A,B,C,D,E,F,G,H,I,J,K,L,M,N,O);
array_web_service!(16,A,B,C,D,E,F,G,H,I,J,K,L,M,N,O,P);

tuple_web_service!((0,A));
tuple_web_service!((0,A),(1,B));
tuple_web_service!((0,A),(1,B),(2,C));
tuple_web_service!((0,A),(1,B),(2,C),(3,D));
tuple_web_service!((0,A),(1,B),(2,C),(3,D),(4,E));
tuple_web_service!((0,A),(1,B),(2,C),(3,D),(4,E),(5,F));
tuple_web_service!((0,A),(1,B),(2,C),(3,D),(4,E),(5,F),(6,G));
tuple_web_service!((0,A),(1,B),(2,C),(3,D),(4,E),(5,F),(6,G),(7,H));
tuple_web_service!((0,A),(1,B),(2,C),(3,D),(4,E),(5,F),(6,G),(7,H),(8,I));
tuple_web_service!((0,A),(1,B),(2,C),(3,D),(4,E),(5,F),(6,G),(7,H),(8,I),(9,J));
tuple_web_service!((0,A),(1,B),(2,C),(3,D),(4,E),(5,F),(6,G),(7,H),(8,I),(9,J),(10,K));
tuple_web_service!((0,A),(1,B),(2,C),(3,D),(4,E),(5,F),(6,G),(7,H),(8,I),(9,J),(10,K),(11,L));
tuple_web_service!((0,A),(1,B),(2,C),(3,D),(4,E),(5,F),(6,G),(7,H),(8,I),(9,J),(10,K),(11,L),(12,M));
tuple_web_service!((0,A),(1,B),(2,C),(3,D),(4,E),(5,F),(6,G),(7,H),(8,I),(9,J),(10,K),(11,L),(12,M),(13,N));
tuple_web_service!((0,A),(1,B),(2,C),(3,D),(4,E),(5,F),(6,G),(7,H),(8,I),(9,J),(10,K),(11,L),(12,M),(13,N),(14,O));
tuple_web_service!((0,A),(1,B),(2,C),(3,D),(4,E),(5,F),(6,G),(7,H),(8,I),(9,J),(10,K),(11,L),(12,M),(13,N),(14,O),(15,P));
tuple_web_service!((0,A),(1,B),(2,C),(3,D),(4,E),(5,F),(6,G),(7,H),(8,I),(9,J),(10,K),(11,L),(12,M),(13,N),(14,O),(15,P),(16,Q));
tuple_web_service!((0,A),(1,B),(2,C),(3,D),(4,E),(5,F),(6,G),(7,H),(8,I),(9,J),(10,K),(11,L),(12,M),(13,N),(14,O),(15,P),(16,Q),(17,R));
tuple_web_service!((0,A),(1,B),(2,C),(3,D),(4,E),(5,F),(6,G),(7,H),(8,I),(9,J),(10,K),(11,L),(12,M),(13,N),(14,O),(15,P),(16,Q),(17,R),(18,S));
tuple_web_service!((0,A),(1,B),(2,C),(3,D),(4,E),(5,F),(6,G),(7,H),(8,I),(9,J),(10,K),(11,L),(12,M),(13,N),(14,O),(15,P),(16,Q),(17,R),(18,S),(19,T));
tuple_web_service!((0,A),(1,B),(2,C),(3,D),(4,E),(5,F),(6,G),(7,H),(8,I),(9,J),(10,K),(11,L),(12,M),(13,N),(14,O),(15,P),(16,Q),(17,R),(18,S),(19,T),(20,V));
tuple_web_service!((0,A),(1,B),(2,C),(3,D),(4,E),(5,F),(6,G),(7,H),(8,I),(9,J),(10,K),(11,L),(12,M),(13,N),(14,O),(15,P),(16,Q),(17,R),(18,S),(19,T),(20,V),(21,X));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Method, StatusCode};
    use crate::service::Service;
    use crate::web::test::{init_service, TestRequest};
    use crate::web::{self, guard, App, DefaultError, HttpResponse};

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
        let s = format!("{:?}", req);
        assert!(s.contains("WebRequest"));
        assert!(s.contains("test=1"));
        assert!(s.contains("x-test"));

        let res = HttpResponse::Ok().header("x-test", "111").finish();
        let res = TestRequest::post()
            .uri("/index.html?test=1")
            .to_srv_response(res);

        let s = format!("{:?}", res);
        assert!(s.contains("WebResponse"));
        assert!(s.contains("x-test"));
    }
}
