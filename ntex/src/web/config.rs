use std::{net::SocketAddr, rc::Rc};

use crate::{router::ResourceDef, util::Extensions};

use super::resource::Resource;
use super::route::Route;
use super::service::{AppServiceFactory, ServiceFactoryWrapper, WebServiceFactory};
use super::{DefaultError, ErrorRenderer};

/// Application configuration
#[derive(Debug, Clone)]
pub struct AppConfig(Rc<AppConfigInner>);

#[derive(Debug)]
struct AppConfigInner {
    secure: bool,
    host: String,
    addr: SocketAddr,
}

impl AppConfig {
    /// Create an AppConfig instance.
    pub fn new(secure: bool, addr: SocketAddr, host: String) -> Self {
        AppConfig(Rc::new(AppConfigInner { secure, host, addr }))
    }

    /// Server host name.
    ///
    /// Host name is used by application router as a hostname for url generation.
    /// Check [ConnectionInfo](./struct.ConnectionInfo.html#method.host)
    /// documentation for more information.
    ///
    /// By default host name is set to a "localhost" value.
    pub fn host(&self) -> &str {
        &self.0.host
    }

    /// Returns true if connection is secure(https)
    pub fn secure(&self) -> bool {
        self.0.secure
    }

    /// Returns the socket address of the local half of this TCP connection
    pub fn local_addr(&self) -> SocketAddr {
        self.0.addr
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig::new(
            false,
            "127.0.0.1:8080".parse().unwrap(),
            "localhost:8080".to_owned(),
        )
    }
}

/// Service config is used for external configuration.
///
/// Part of application configuration could be offloaded
/// to set of external methods. This could help with
/// modularization of big application configuration.
pub struct ServiceConfig<Err = DefaultError> {
    pub(super) services: Vec<Box<dyn AppServiceFactory<Err>>>,
    pub(super) state: Extensions,
    pub(super) external: Vec<ResourceDef>,
}

impl<Err: ErrorRenderer> ServiceConfig<Err> {
    pub fn new() -> Self {
        Self {
            services: Vec::new(),
            state: Extensions::new(),
            external: Vec::new(),
        }
    }

    /// Set application state.
    ///
    /// This is same as `App::state()` method.
    pub fn state<S: 'static>(&mut self, st: S) -> &mut Self {
        self.state.insert(st);
        self
    }

    /// Configure route for a specific path.
    ///
    /// This is same as `App::route()` method.
    pub fn route(&mut self, path: &str, mut route: Route<Err>) -> &mut Self {
        self.service(
            Resource::new(path)
                .add_guards(route.take_guards())
                .route(route),
        )
    }

    /// Register http service.
    ///
    /// This is same as `App::service()` method.
    pub fn service<F>(&mut self, factory: F) -> &mut Self
    where
        F: WebServiceFactory<Err> + 'static,
    {
        self.services
            .push(Box::new(ServiceFactoryWrapper::new(factory)));
        self
    }

    /// Register an external resource.
    ///
    /// External resources are useful for URL generation purposes only
    /// and are never considered for matching at request time. Calls to
    /// `HttpRequest::url_for()` will work as expected.
    ///
    /// This is same as `App::external_service()` method.
    pub fn external_resource<N, U>(&mut self, name: N, url: U) -> &mut Self
    where
        N: AsRef<str>,
        U: AsRef<str>,
    {
        let mut rdef = ResourceDef::new(url.as_ref());
        *rdef.name_mut() = name.as_ref().to_string();
        self.external.push(rdef);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Method, StatusCode};
    use crate::util::Bytes;
    use crate::web::test::{TestRequest, call_service, init_service, read_body};
    use crate::web::{self, App, DefaultError, HttpRequest, HttpResponse};

    #[crate::rt_test]
    async fn test_configure_state() {
        let cfg = |cfg: &mut ServiceConfig<_>| {
            cfg.state(10usize);
        };

        let srv = init_service(
            App::new().configure(cfg).service(
                web::resource("/")
                    .to(|_: web::types::State<usize>| async { HttpResponse::Ok() }),
            ),
        )
        .await;
        let req = TestRequest::default().to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[cfg(feature = "url")]
    #[crate::rt_test]
    async fn test_configure_external_resource() {
        let srv = init_service(
            App::new()
                .configure(|cfg| {
                    cfg.external_resource(
                        "youtube",
                        "https://youtube.com/watch/{video_id}",
                    );
                })
                .route(
                    "/test",
                    web::get().to(|req: HttpRequest| async move {
                        HttpResponse::Ok()
                            .body(format!("{}", req.url_for("youtube", ["12345"]).unwrap()))
                    }),
                ),
        )
        .await;
        let req = TestRequest::with_uri("/test").to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = read_body(resp).await;
        assert_eq!(body, Bytes::from_static(b"https://youtube.com/watch/12345"));
    }

    #[crate::rt_test]
    async fn test_configure_service() {
        let srv = init_service(App::new().configure(|cfg| {
            cfg.service(
                web::resource("/test")
                    .route(web::get().to(|| async { HttpResponse::Created() })),
            )
            .route(
                "/index.html",
                web::get().to(|| async { HttpResponse::Ok() }),
            );
        }))
        .await;

        let req = TestRequest::with_uri("/test")
            .method(Method::GET)
            .to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::CREATED);

        let req = TestRequest::with_uri("/index.html")
            .method(Method::GET)
            .to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn test_new_service_config() {
        let cfg: ServiceConfig<DefaultError> = ServiceConfig::new();
        assert!(cfg.services.is_empty());
        assert!(cfg.external.is_empty());
    }
}
