use std::{any::Any, any::TypeId, cell::UnsafeCell, net::SocketAddr, rc::Rc};

use crate::service::cfg::{CfgContext, Configuration};
use crate::{router::ResourceDef, util::ByteString, util::HashMap};

use super::httprequest::{HttpRequest, HttpRequestInner};
use super::service::{AppServiceFactory, ServiceFactoryWrapper, WebServiceFactory};
use super::{AppState, Resource, Route};

/// Application configuration
#[derive(Debug)]
pub struct WebAppConfig {
    name: ByteString,
    secure: bool,
    host: String,
    addr: SocketAddr,
    config: CfgContext,
    extensions: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
    pub(super) pool_size: usize,
}

impl Default for WebAppConfig {
    fn default() -> Self {
        WebAppConfig::new()
    }
}

impl Configuration for WebAppConfig {
    const NAME: &str = "Web app configuration";

    fn ctx(&self) -> &CfgContext {
        &self.config
    }

    fn set_ctx(&mut self, ctx: CfgContext) {
        self.config = ctx;
    }
}

impl WebAppConfig {
    #[must_use]
    /// Create an default `WebAppConfig` instance.
    pub fn new() -> Self {
        WebAppConfig::with(
            "ntex:web",
            false,
            "127.0.0.1:8080".parse().unwrap(),
            "localhost:8080".to_owned(),
        )
    }

    #[must_use]
    /// Create an `WebAppConfig` instance.
    pub fn with(name: &str, secure: bool, addr: SocketAddr, host: String) -> Self {
        WebAppConfig {
            secure,
            host,
            addr,
            name: name.into(),
            pool_size: 128,
            config: CfgContext::default(),
            extensions: HashMap::default(),
        }
    }

    /// Server name
    pub fn name(&self) -> &ByteString {
        &self.name
    }

    /// Server host name
    ///
    /// Host name is used by application router as a hostname for url generation.
    /// Check [`ConnectionInfo`](./struct.ConnectionInfo.html#method.host)
    /// documentation for more information.
    ///
    /// By default host name is set to a "localhost" value.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Returns true if connection is secure(https)
    pub fn secure(&self) -> bool {
        self.secure
    }

    /// Returns the socket address of the local half of this TCP connection
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Set application level arbitrary state item.
    ///
    /// Application state is available
    /// via `HttpRequest::app_state()` method at runtime.
    pub fn state<T: 'static>(&self) -> Option<&T> {
        self.extensions
            .get(&TypeId::of::<T>())
            .and_then(|boxed| boxed.downcast_ref())
    }

    #[must_use]
    /// Set server host name.
    ///
    /// By default host name is set to a "localhost" value.
    pub fn set_host(mut self, host: String) -> Self {
        self.host = host;
        self
    }

    #[must_use]
    /// Connection is secure(https).
    pub fn set_secure(mut self) -> Self {
        self.secure = true;
        self
    }

    #[must_use]
    /// Returns the socket address of the local half of this TCP connection.
    pub fn set_local_addr(mut self, addr: SocketAddr) -> Self {
        self.addr = addr;
        self
    }

    #[must_use]
    /// Set size of `HttpRequest` pool size.
    ///
    /// By default pool size is 128.
    pub fn set_pool_size(mut self, size: usize) -> Self {
        self.pool_size = size;
        self
    }

    #[must_use]
    /// Set application level arbitrary state item.
    pub fn set_state<T: Sync + Send + 'static>(mut self, val: T) -> Self {
        self.extensions
            .insert(TypeId::of::<T>(), Box::new(val))
            .and_then(|item| item.downcast::<T>().map(|boxed| *boxed).ok());
        self
    }

    /// Get message from the pool.
    pub(crate) fn get_request(&self) -> Option<HttpRequest> {
        CACHE.with(|cache| cache.with(self.config.id(), |cache| cache.pop().map(HttpRequest)))
    }
}

/// Put message from the pool.
pub(crate) fn put_request(id: usize, pool_size: usize, req: &mut Rc<HttpRequestInner>) {
    CACHE.with(|cache| {
        cache.with(id, |cache| {
            if cache.len() < pool_size
                && let Some(inner) = Rc::get_mut(req)
            {
                inner.head.remove_io();
                inner.head.extensions.borrow_mut().clear();
                cache.push(req.clone());
            }
        });
    });
}

/// Service config is used for external configuration.
///
/// Part of application configuration could be offloaded
/// to set of external methods. This could help with
/// modularization of big application configuration.
#[derive(derive_more::Debug)]
#[debug("ServiceConfig")]
pub struct ServiceConfig<St, Cfg> {
    pub(super) services: Vec<Box<dyn AppServiceFactory<St, Cfg>>>,
    pub(super) external: Vec<ResourceDef>,
}

impl<St: AppState, Cfg> ServiceConfig<St, Cfg>
where
    Cfg: Clone + 'static,
{
    pub fn new() -> Self {
        Self {
            services: Vec::new(),
            external: Vec::new(),
        }
    }

    /// Configure route for a specific path.
    ///
    /// This is same as `App::route()` method.
    pub fn route(&mut self, path: &str, mut route: Route<St>) -> &mut Self {
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
        F: WebServiceFactory<St, Cfg> + 'static,
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
    pub fn external_resource(&mut self, name: impl AsRef<str>, url: impl AsRef<str>) -> &mut Self {
        let mut rdef = ResourceDef::new(url.as_ref());
        *rdef.name_mut() = name.as_ref().to_string();
        self.external.push(rdef);
        self
    }
}

impl<St: AppState, Cfg: Clone + 'static> Default for ServiceConfig<St, Cfg> {
    fn default() -> Self {
        Self::new()
    }
}

thread_local! {
    static CACHE: LocalCache = LocalCache::new();
}

/// Request's objects pool
struct LocalCache {
    cache: UnsafeCell<Vec<Vec<Rc<HttpRequestInner>>>>,
}

impl LocalCache {
    fn new() -> Self {
        Self {
            cache: UnsafeCell::new(Vec::with_capacity(16)),
        }
    }

    fn with<F, R>(&self, idx: usize, f: F) -> R
    where
        F: FnOnce(&mut Vec<Rc<HttpRequestInner>>) -> R,
    {
        let cache = unsafe { &mut *self.cache.get() };

        while cache.len() <= idx {
            cache.push(Vec::new());
        }
        f(&mut cache[idx])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Method, StatusCode};
    use crate::util::Bytes;
    use crate::web::test::{TestRequest, call_service, init_service, read_body};
    use crate::web::{self, App, HttpRequest, HttpResponse};

    #[crate::rt_test]
    async fn test_webappconfig() {
        let cfg = WebAppConfig::default()
            .set_host("www.example.org".to_string())
            .set_local_addr("127.0.0.1:8080".parse().unwrap())
            .set_pool_size(256);
        assert_eq!(cfg.host(), "www.example.org");
        assert_eq!(cfg.local_addr(), "127.0.0.1:8080".parse().unwrap());
        assert_eq!(cfg.pool_size, 256);
    }

    #[cfg(feature = "url")]
    #[crate::rt_test]
    async fn test_configure_external_resource() {
        let srv = init_service(
            App::new()
                .configure(|cfg| {
                    cfg.external_resource("youtube", "https://youtube.com/watch/{video_id}");
                })
                .route(
                    "/test",
                    web::get().to(async move |req: HttpRequest| {
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
                web::resource("/test").route(web::get().to(async || HttpResponse::Created())),
            )
            .route("/index.html", web::get().to(async || HttpResponse::Ok()));
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
        let cfg: ServiceConfig<(), ()> = ServiceConfig::default();
        assert!(cfg.services.is_empty());
        assert!(cfg.external.is_empty());
    }
}
