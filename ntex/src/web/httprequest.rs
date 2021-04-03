use std::{cell::Ref, cell::RefCell, cell::RefMut, fmt, net, rc::Rc};

use crate::http::{
    HeaderMap, HttpMessage, Message, Method, Payload, RequestHead, Uri, Version,
};
use crate::router::Path;
use crate::util::{Extensions, Ready};

use super::config::AppConfig;
use super::error::ErrorRenderer;
use super::extract::FromRequest;
use super::info::ConnectionInfo;
use super::rmap::ResourceMap;

#[derive(Clone)]
/// An HTTP Request
pub struct HttpRequest(pub(crate) Rc<HttpRequestInner>);

pub(crate) struct HttpRequestInner {
    pub(crate) head: Message<RequestHead>,
    pub(crate) path: Path<Uri>,
    pub(crate) payload: Payload,
    pub(crate) app_data: Rc<Extensions>,
    rmap: Rc<ResourceMap>,
    config: AppConfig,
    pool: &'static HttpRequestPool,
}

impl HttpRequest {
    #[inline]
    pub(crate) fn new(
        path: Path<Uri>,
        head: Message<RequestHead>,
        payload: Payload,
        rmap: Rc<ResourceMap>,
        config: AppConfig,
        app_data: Rc<Extensions>,
        pool: &'static HttpRequestPool,
    ) -> HttpRequest {
        HttpRequest(Rc::new(HttpRequestInner {
            head,
            path,
            payload,
            rmap,
            config,
            app_data,
            pool,
        }))
    }
}

impl HttpRequest {
    /// This method returns reference to the request head
    #[inline]
    pub fn head(&self) -> &RequestHead {
        &self.0.head
    }

    /// This method returns muttable reference to the request head.
    /// panics if multiple references of http request exists.
    #[inline]
    pub(crate) fn head_mut(&mut self) -> &mut RequestHead {
        &mut Rc::get_mut(&mut self.0).unwrap().head
    }

    /// Request's uri.
    #[inline]
    pub fn uri(&self) -> &Uri {
        &self.head().uri
    }

    /// Read the Request method.
    #[inline]
    pub fn method(&self) -> &Method {
        &self.head().method
    }

    /// Read the Request Version.
    #[inline]
    pub fn version(&self) -> Version {
        self.head().version
    }

    #[inline]
    /// Returns request's headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.head().headers
    }

    /// The target path of this Request.
    #[inline]
    pub fn path(&self) -> &str {
        self.head().uri.path()
    }

    /// The query string in the URL.
    ///
    /// E.g., id=10
    #[inline]
    pub fn query_string(&self) -> &str {
        if let Some(query) = self.uri().query().as_ref() {
            query
        } else {
            ""
        }
    }

    /// Get a reference to the Path parameters.
    ///
    /// Params is a container for url parameters.
    /// A variable segment is specified in the form `{identifier}`,
    /// where the identifier can be used later in a request handler to
    /// access the matched value for that segment.
    #[inline]
    pub fn match_info(&self) -> &Path<Uri> {
        &self.0.path
    }

    #[inline]
    pub(crate) fn match_info_mut(&mut self) -> &mut Path<Uri> {
        &mut Rc::get_mut(&mut self.0).unwrap().path
    }

    /// Request extensions
    #[inline]
    pub fn extensions(&self) -> Ref<'_, Extensions> {
        self.head().extensions()
    }

    /// Mutable reference to a the request's extensions
    #[inline]
    pub fn extensions_mut(&self) -> RefMut<'_, Extensions> {
        self.head().extensions_mut()
    }

    #[cfg(feature = "url")]
    /// Generate url for named resource
    ///
    /// ```rust
    /// # use ntex::web::{self, App, HttpRequest, HttpResponse};
    /// #
    /// async fn index(req: HttpRequest) -> HttpResponse {
    ///     let url = req.url_for("foo", &["1", "2", "3"]); // <- generate url for "foo" resource
    ///     HttpResponse::Ok().into()
    /// }
    ///
    /// fn main() {
    ///     let app = App::new()
    ///         .service(web::resource("/test/{one}/{two}/{three}")
    ///              .name("foo")  // <- set resource name, then it could be used in `url_for`
    ///              .route(web::get().to(index))
    ///         );
    /// }
    /// ```
    pub fn url_for<U, I>(
        &self,
        name: &str,
        elements: U,
    ) -> Result<url_pkg::Url, super::error::UrlGenerationError>
    where
        U: IntoIterator<Item = I>,
        I: AsRef<str>,
    {
        self.0.rmap.url_for(&self, name, elements)
    }

    #[cfg(feature = "url")]
    /// Generate url for named resource
    ///
    /// This method is similar to `HttpRequest::url_for()` but it can be used
    /// for urls that do not contain variable parts.
    pub fn url_for_static(
        &self,
        name: &str,
    ) -> Result<url_pkg::Url, super::error::UrlGenerationError> {
        const NO_PARAMS: [&str; 0] = [];
        self.url_for(name, &NO_PARAMS)
    }

    #[inline]
    /// Get a reference to a `ResourceMap` of current application.
    pub fn resource_map(&self) -> &ResourceMap {
        &self.0.rmap
    }

    /// Peer socket address
    ///
    /// Peer address is actual socket address, if proxy is used in front of
    /// ntex http server, then peer address would be address of this proxy.
    ///
    /// To get client connection information `.connection_info()` should be used.
    #[inline]
    pub fn peer_addr(&self) -> Option<net::SocketAddr> {
        self.head().peer_addr
    }

    /// Get *ConnectionInfo* for the current request.
    ///
    /// This method panics if request's extensions container is already
    /// borrowed.
    #[inline]
    pub fn connection_info(&self) -> Ref<'_, ConnectionInfo> {
        ConnectionInfo::get(self.head(), &*self.app_config())
    }

    /// App config
    #[inline]
    pub fn app_config(&self) -> &AppConfig {
        &self.0.config
    }

    /// Get an application data object stored with `App::data` or `App::app_data`
    /// methods during application configuration.
    ///
    /// If `App::data` was used to store object, use `Data<T>`:
    ///
    /// ```rust,ignore
    /// let opt_t = req.app_data::<Data<T>>();
    /// ```
    pub fn app_data<T: 'static>(&self) -> Option<&T> {
        if let Some(st) = self.0.app_data.get::<T>() {
            Some(&st)
        } else {
            None
        }
    }
}

impl HttpMessage for HttpRequest {
    #[inline]
    /// Returns Request's headers.
    fn message_headers(&self) -> &HeaderMap {
        &self.head().headers
    }

    /// Request extensions
    #[inline]
    fn message_extensions(&self) -> Ref<'_, Extensions> {
        self.0.head.extensions()
    }

    /// Mutable reference to a the request's extensions
    #[inline]
    fn message_extensions_mut(&self) -> RefMut<'_, Extensions> {
        self.0.head.extensions_mut()
    }
}

impl Drop for HttpRequest {
    fn drop(&mut self) {
        if Rc::strong_count(&self.0) == 1 {
            let v = &mut self.0.pool.0.borrow_mut();
            if v.len() < 128 {
                self.extensions_mut().clear();
                v.push(self.0.clone());
            }
        }
    }
}

/// It is possible to get `HttpRequest` as an extractor handler parameter
///
/// ## Example
///
/// ```rust
/// use ntex::web::{self, App, HttpRequest};
///
/// /// extract `HttpRequest` from request
/// async fn index(req: HttpRequest) -> String {
///    format!("Got thing: {:?}", req)
/// }
///
/// fn main() {
///     let app = App::new().service(
///         web::resource("/users/{first}").route(
///             web::get().to(index))
///     );
/// }
/// ```
impl<Err: ErrorRenderer> FromRequest<Err> for HttpRequest {
    type Error = Err::Container;
    type Future = Ready<Self, Self::Error>;

    #[inline]
    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        Ok(req.clone()).into()
    }
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "\nHttpRequest {:?} {}:{}",
            self.0.head.version,
            self.0.head.method,
            self.path()
        )?;
        if !self.query_string().is_empty() {
            writeln!(f, "  query: ?{:?}", self.query_string())?;
        }
        if !self.match_info().is_empty() {
            writeln!(f, "  params: {:?}", self.match_info())?;
        }
        writeln!(f, "  headers:")?;
        for (key, val) in self.headers().iter() {
            writeln!(f, "    {:?}: {:?}", key, val)?;
        }
        Ok(())
    }
}

/// Request's objects pool
pub(crate) struct HttpRequestPool(RefCell<Vec<Rc<HttpRequestInner>>>);

impl HttpRequestPool {
    pub(crate) fn create() -> &'static HttpRequestPool {
        let pool = HttpRequestPool(RefCell::new(Vec::with_capacity(128)));
        Box::leak(Box::new(pool))
    }

    /// Get message from the pool
    #[inline]
    pub(crate) fn get_request(&self) -> Option<HttpRequest> {
        if let Some(inner) = self.0.borrow_mut().pop() {
            Some(HttpRequest(inner))
        } else {
            None
        }
    }

    pub(crate) fn clear(&self) {
        self.0.borrow_mut().clear()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{header, StatusCode};
    use crate::router::ResourceDef;
    use crate::web::dev::ResourceMap;
    use crate::web::test::{call_service, init_service, TestRequest};
    use crate::web::{self, App, HttpResponse};

    #[test]
    fn test_debug() {
        let req =
            TestRequest::with_header("content-type", "text/plain").to_http_request();
        let dbg = format!("{:?}", req);
        assert!(dbg.contains("HttpRequest"));

        let req = TestRequest::with_uri("/index.html?q=?").to_http_request();
        let dbg = format!("{:?}", req);
        assert!(dbg.contains("HttpRequest"));
        assert!(req.peer_addr().is_none());
        assert_eq!(req.method(), &Method::GET);
    }

    #[cfg(feature = "cookie")]
    #[test]
    fn test_no_request_cookies() {
        let req = TestRequest::default().to_http_request();
        assert!(req.cookies().unwrap().is_empty());
    }

    #[cfg(feature = "cookie")]
    #[test]
    fn test_request_cookies() {
        let req = TestRequest::default()
            .header(header::COOKIE, "cookie1=value1")
            .header(header::COOKIE, "cookie2=value2")
            .to_http_request();
        {
            let cookies = req.cookies().unwrap();
            assert_eq!(cookies.len(), 2);
            assert_eq!(cookies[0].name(), "cookie2");
            assert_eq!(cookies[0].value(), "value2");
            assert_eq!(cookies[1].name(), "cookie1");
            assert_eq!(cookies[1].value(), "value1");
        }

        let cookie = req.cookie("cookie1");
        assert!(cookie.is_some());
        let cookie = cookie.unwrap();
        assert_eq!(cookie.name(), "cookie1");
        assert_eq!(cookie.value(), "value1");

        let cookie = req.cookie("cookie-unknown");
        assert!(cookie.is_none());
    }

    #[test]
    fn test_request_query() {
        let req = TestRequest::with_uri("/?id=test").to_http_request();
        assert_eq!(req.query_string(), "id=test");
    }

    #[cfg(feature = "url")]
    #[test]
    fn test_url_for() {
        let mut res = ResourceDef::new("/user/{name}.{ext}");
        *res.name_mut() = "index".to_string();

        let mut rmap = ResourceMap::new(ResourceDef::new(""));
        rmap.add(&mut res, None);
        //assert!(rmap.has_resource("/user/test.html"));
        //assert!(!rmap.has_resource("/test/unknown"));

        let req = TestRequest::with_header(header::HOST, "www.rust-lang.org")
            .rmap(rmap)
            .to_http_request();

        assert_eq!(
            req.url_for("unknown", &["test"]),
            Err(crate::web::error::UrlGenerationError::ResourceNotFound)
        );
        assert_eq!(
            req.url_for("index", &["test"]),
            Err(crate::web::error::UrlGenerationError::NotEnoughElements)
        );
        let url = req.url_for("index", &["test", "html"]);
        assert_eq!(
            url.ok().unwrap().as_str(),
            "http://www.rust-lang.org/user/test.html"
        );
    }

    #[cfg(feature = "url")]
    #[test]
    fn test_url_for_static() {
        let mut rdef = ResourceDef::new("/index.html");
        *rdef.name_mut() = "index".to_string();

        let mut rmap = ResourceMap::new(ResourceDef::new(""));
        rmap.add(&mut rdef, None);

        // assert!(rmap.has_resource("/index.html"));

        let req = TestRequest::with_uri("/test")
            .header(header::HOST, "www.rust-lang.org")
            .rmap(rmap)
            .to_http_request();
        let url = req.url_for_static("index");
        assert_eq!(
            url.ok().unwrap().as_str(),
            "http://www.rust-lang.org/index.html"
        );
    }

    #[cfg(feature = "url")]
    #[test]
    fn test_url_for_external() {
        let mut rdef = ResourceDef::new("https://youtube.com/watch/{video_id}");

        *rdef.name_mut() = "youtube".to_string();

        let mut rmap = ResourceMap::new(ResourceDef::new(""));
        rmap.add(&mut rdef, None);
        // assert!(rmap.has_resource("https://youtube.com/watch/unknown"));

        let req = TestRequest::default().rmap(rmap).to_http_request();
        let url = req.url_for("youtube", &["oHg5SJYRHA0"]);
        assert_eq!(
            url.ok().unwrap().as_str(),
            "https://youtube.com/watch/oHg5SJYRHA0"
        );
    }

    #[crate::rt_test]
    async fn test_data() {
        let srv = init_service(App::new().app_data(10usize).service(
            web::resource("/").to(|req: HttpRequest| async move {
                if req.app_data::<usize>().is_some() {
                    HttpResponse::Ok()
                } else {
                    HttpResponse::BadRequest()
                }
            }),
        ))
        .await;

        let req = TestRequest::default().to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let srv = init_service(App::new().app_data(10u32).service(
            web::resource("/").to(|req: HttpRequest| async move {
                if req.app_data::<usize>().is_some() {
                    HttpResponse::Ok()
                } else {
                    HttpResponse::BadRequest()
                }
            }),
        ))
        .await;

        let req = TestRequest::default().to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[crate::rt_test]
    async fn test_extensions_dropped() {
        struct Tracker {
            dropped: bool,
        }
        struct Foo {
            tracker: Rc<RefCell<Tracker>>,
        }
        impl Drop for Foo {
            fn drop(&mut self) {
                self.tracker.borrow_mut().dropped = true;
            }
        }

        let tracker = Rc::new(RefCell::new(Tracker { dropped: false }));
        {
            let tracker2 = Rc::clone(&tracker);
            let srv = init_service(App::new().data(10u32).service(
                web::resource("/").to(move |req: HttpRequest| {
                    req.extensions_mut().insert(Foo {
                        tracker: Rc::clone(&tracker2),
                    });
                    async { HttpResponse::Ok() }
                }),
            ))
            .await;

            let req = TestRequest::default().to_request();
            let resp = call_service(&srv, req).await;
            assert_eq!(resp.status(), StatusCode::OK);
        }

        assert!(tracker.borrow().dropped);
    }
}
