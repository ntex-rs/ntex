use std::task::{Context, Poll};
use std::{convert::Infallible, future::Future, marker::PhantomData, pin::Pin, rc::Rc};

use crate::router::{IntoPattern, ResourceDef};
use crate::service::{IntoServiceFactory, Service, ServiceFactory};
use crate::util::Extensions;

use super::boxed::{self, BoxServiceFactory};
use super::config::AppConfig;
use super::dev::insert_slash;
use super::rmap::ResourceMap;
use super::types::state::StateFactory;
use super::{guard::Guard, ErrorRenderer, WebRequest, WebResponse};

pub trait WebService<'a, Err: ErrorRenderer>: 'static {
    fn register(self, config: &mut WebServiceConfig<'a, Err>);
}

pub(super) trait WebServiceWrapper<'a, Err: ErrorRenderer> {
    fn register(&mut self, config: &mut WebServiceConfig<'a, Err>);
}

pub(super) fn create_web_service<'a, T, Err>(svc: T) -> Box<dyn WebServiceWrapper<'a, Err>>
where
    T: WebService<'a, Err> + 'static,
    Err: ErrorRenderer,
{
    Box::new(WebServiceWrapperInner(Some(svc)))
}

struct WebServiceWrapperInner<T>(Option<T>);

impl<'a, T, Err> WebServiceWrapper<'a, Err> for WebServiceWrapperInner<T>
where
    T: WebService<'a, Err> + 'static,
    Err: ErrorRenderer,
{
    fn register(&mut self, config: &mut WebServiceConfig<'a, Err>) {
        if let Some(item) = self.0.take() {
            item.register(config)
        }
    }
}

type Guards = Vec<Box<dyn Guard>>;

/// Application service configuration
pub struct WebServiceConfig<'a, Err: ErrorRenderer> {
    config: AppConfig,
    root: bool,
    default: Rc<BoxServiceFactory<'a, Err>>,
    services: Vec<(
        ResourceDef,
        BoxServiceFactory<'a, Err>,
        Option<Guards>,
        Option<Rc<ResourceMap>>,
    )>,
    service_state: Rc<Vec<Box<dyn StateFactory>>>,
}

impl<'a, Err: ErrorRenderer> WebServiceConfig<'a, Err> {
    /// Crate server settings instance
    pub(crate) fn new(
        config: AppConfig,
        default: Rc<BoxServiceFactory<'a, Err>>,
        service_state: Rc<Vec<Box<dyn StateFactory>>>,
    ) -> Self {
        WebServiceConfig {
            config,
            default,
            service_state,
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
            BoxServiceFactory<'a, Err>,
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
            service_state: self.service_state.clone(),
        }
    }

    /// Service configuration
    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    /// Default resource
    pub fn default_service(&self) -> Rc<BoxServiceFactory<'a, Err>> {
        self.default.clone()
    }

    /// Set global route state
    pub fn set_service_state(&self, extensions: &mut Extensions) -> bool {
        for f in self.service_state.iter() {
            f.create(extensions);
        }
        !self.service_state.is_empty()
    }

    /// Register http service
    pub fn register_service<S>(
        &mut self,
        rdef: ResourceDef,
        guards: Option<Vec<Box<dyn Guard>>>,
        factory: S,
        nested: Option<Rc<ResourceMap>>,
    ) where
        S: ServiceFactory<
                &'a mut WebRequest<'a, Err>,
                Response = WebResponse,
                Error = Err::Container,
                InitError = (),
            > + 'static,
        S::Future: 'static,
        S::Service: 'static,
    {
        self.services
            .push((rdef, boxed::factory(factory), guards, nested));
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
    pub fn guard<G: Guard + 'static>(mut self, guard: G) -> Self {
        self.guards.push(Box::new(guard));
        self
    }

    // /// Set a service factory implementation and generate web service.
    // pub fn finish<'a, T, F, Err>(self, service: F) -> impl WebServiceFactory<'a, Err>
    // where
    //     F: IntoServiceFactory<T, &'a mut WebRequest<'a, Err>>,
    //     T: ServiceFactory<
    //             &'a mut WebRequest<'a, Err>,
    //             Response = WebResponse,
    //             Error = Err::Container,
    //             InitError = (),
    //         > + 'static,
    //     Err: ErrorRenderer,
    // {
    //     WebServiceImpl {
    //         srv: service.into_factory(),
    //         rdef: self.rdef,
    //         name: self.name,
    //         guards: self.guards,
    //         _t: PhantomData,
    //     }
    // }
}

// struct WebServiceImpl<T> {
//     srv: T,
//     rdef: Vec<String>,
//     name: Option<String>,
//     guards: Vec<Box<dyn Guard>>,
// }

// impl<'a, T, Err> WebServiceFactory<'a, Err> for WebServiceImpl<T>
// where
//     T: ServiceFactory<
//             &'a mut WebRequest<'a, Err>,
//             Response = WebResponse,
//             Error = Err::Container,
//             InitError = (),
//         > + 'static,
//     T::Future: 'static,
//     T::Service: 'static,
//     Err: ErrorRenderer,
// {
//     fn register(mut self, config: &mut WebServiceConfig<'a, Err>) {
//         let guards = if self.guards.is_empty() {
//             None
//         } else {
//             Some(std::mem::take(&mut self.guards))
//         };

//         let mut rdef = if config.is_root() || !self.rdef.is_empty() {
//             ResourceDef::new(insert_slash(self.rdef))
//         } else {
//             ResourceDef::new(self.rdef)
//         };
//         if let Some(ref name) = self.name {
//             *rdef.name_mut() = name.clone();
//         }
//         config.register_service(rdef, guards, self.srv, None)
//     }
// }

// /// WebServiceFactory implementation for a Vec<T>
// #[allow(unused_parens)]
// impl<'a, Err, T> WebServiceFactory<'a, Err> for Vec<T>
// where
//     Err: ErrorRenderer,
//     T: WebServiceFactory<'a, Err> + 'static,
// {
//     fn register(mut self, config: &mut WebServiceConfig<'a, Err>) {
//         for service in self.drain(..) {
//             service.register(config);
//         }
//     }
// }

macro_rules! tuple_web_service({$(($n:tt, $T:ident)),+} => {
    /// WebServiceFactory implementation for a tuple
    #[allow(unused_parens)]
    impl<'a, Err: ErrorRenderer, $($T: WebService<'a, Err> + 'static),+> WebService<'a, Err> for ($($T,)+) {
        fn register(self, config: &mut WebServiceConfig<'a, Err>) {
            $(
                self.$n.register(config);
            )+
        }
    }
});

macro_rules! array_web_service({$num:tt, $($T:ident),+} => {
    /// WebServiceFactory implementation for an array
    #[allow(unused_parens)]
    impl<'a, Err, T> WebService<'a, Err> for [T; $num]
    where
        Err: ErrorRenderer,
        T: WebService<'a, Err> + 'static,
    {
        fn register(self, config: &mut WebServiceConfig<'a, Err>) {
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
