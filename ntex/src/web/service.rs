use std::cell::{Ref, RefMut};
use std::marker::PhantomData;
use std::rc::Rc;
use std::{fmt, net};

use crate::http::body::{Body, MessageBody, ResponseBody};
use crate::http::{
    Extensions, HeaderMap, HttpMessage, Method, Payload, PayloadStream, RequestHead,
    Response, ResponseHead, StatusCode, Uri, Version,
};
use crate::router::{IntoPattern, Path, Resource, ResourceDef};
use crate::{IntoServiceFactory, ServiceFactory};

use super::config::{AppConfig, AppService};
use super::data::Data;
use super::dev::insert_slesh;
use super::error::ErrorRenderer;
use super::guard::Guard;
use super::info::ConnectionInfo;
use super::request::HttpRequest;
use super::rmap::ResourceMap;

pub trait HttpServiceFactory<Err: ErrorRenderer> {
    fn register(self, config: &mut AppService<Err>);
}

pub(super) trait AppServiceFactory<Err: ErrorRenderer> {
    fn register(&mut self, config: &mut AppService<Err>);
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
    T: HttpServiceFactory<Err>,
    Err: ErrorRenderer,
{
    fn register(&mut self, config: &mut AppService<Err>) {
        if let Some(item) = self.factory.take() {
            item.register(config)
        }
    }
}

/// An service http request
///
/// WebRequest allows mutable access to request's internal structures
pub struct WebRequest<Err> {
    req: HttpRequest,
    _t: PhantomData<Err>,
}

impl<Err: ErrorRenderer> WebRequest<Err> {
    /// Create web response for error
    #[inline]
    pub fn error_response<B, E: Into<Err::Container>>(self, err: E) -> WebResponse<B> {
        WebResponse::from_err::<Err, E>(err, self.req)
    }
}

impl<Err> WebRequest<Err> {
    /// Construct web request
    pub(crate) fn new(req: HttpRequest) -> Self {
        WebRequest {
            req,
            _t: PhantomData,
        }
    }

    /// Deconstruct request into parts
    pub fn into_parts(mut self) -> (HttpRequest, Payload) {
        let pl = Rc::get_mut(&mut (self.req).0).unwrap().payload.take();
        (self.req, pl)
    }

    /// Construct request from parts.
    ///
    /// `WebRequest` can be re-constructed only if `req` hasnt been cloned.
    pub fn from_parts(
        mut req: HttpRequest,
        pl: Payload,
    ) -> Result<Self, (HttpRequest, Payload)> {
        if Rc::strong_count(&req.0) == 1 && Rc::weak_count(&req.0) == 0 {
            Rc::get_mut(&mut req.0).unwrap().payload = pl;
            Ok(WebRequest::new(req))
        } else {
            Err((req, pl))
        }
    }

    /// Construct request from request.
    ///
    /// `HttpRequest` implements `Clone` trait via `Rc` type. `WebRequest`
    /// can be re-constructed only if rc's strong pointers count eq 1 and
    /// weak pointers count is 0.
    pub fn from_request(req: HttpRequest) -> Result<Self, HttpRequest> {
        if Rc::strong_count(&req.0) == 1 && Rc::weak_count(&req.0) == 0 {
            Ok(WebRequest::new(req))
        } else {
            Err(req)
        }
    }

    /// Create web response
    #[inline]
    pub fn into_response<B, R: Into<Response<B>>>(self, res: R) -> WebResponse<B> {
        WebResponse::new(self.req, res.into())
    }

    /// This method returns reference to the request head
    #[inline]
    pub fn head(&self) -> &RequestHead {
        &self.req.head()
    }

    /// This method returns reference to the request head
    #[inline]
    pub fn head_mut(&mut self) -> &mut RequestHead {
        self.req.head_mut()
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

    #[inline]
    /// Returns mutable request's headers.
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.head_mut().headers
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

    /// Peer socket address
    ///
    /// Peer address is actual socket address, if proxy is used in front of
    /// actix http server, then peer address would be address of this proxy.
    ///
    /// To get client connection information `ConnectionInfo` should be used.
    #[inline]
    pub fn peer_addr(&self) -> Option<net::SocketAddr> {
        self.head().peer_addr
    }

    /// Get *ConnectionInfo* for the current request.
    #[inline]
    pub fn connection_info(&self) -> Ref<'_, ConnectionInfo> {
        ConnectionInfo::get(self.head(), &*self.app_config())
    }

    /// Get a reference to the Path parameters.
    ///
    /// Params is a container for url parameters.
    /// A variable segment is specified in the form `{identifier}`,
    /// where the identifier can be used later in a request handler to
    /// access the matched value for that segment.
    #[inline]
    pub fn match_info(&self) -> &Path<Uri> {
        self.req.match_info()
    }

    #[inline]
    /// Get a mutable reference to the Path parameters.
    pub fn match_info_mut(&mut self) -> &mut Path<Uri> {
        self.req.match_info_mut()
    }

    #[inline]
    /// Get a reference to a `ResourceMap` of current application.
    pub fn resource_map(&self) -> &ResourceMap {
        self.req.resource_map()
    }

    /// Service configuration
    #[inline]
    pub fn app_config(&self) -> &AppConfig {
        self.req.app_config()
    }

    #[inline]
    /// Get an application data stored with `App::data()` method during
    /// application configuration.
    pub fn app_data<T: 'static>(&self) -> Option<Data<T>> {
        if let Some(st) = (self.req).0.app_data.get::<Data<T>>() {
            Some(st.clone())
        } else {
            None
        }
    }

    #[inline]
    /// Get request's payload
    pub fn take_payload(&mut self) -> Payload<PayloadStream> {
        Rc::get_mut(&mut (self.req).0).unwrap().payload.take()
    }

    #[inline]
    /// Set request payload.
    pub fn set_payload(&mut self, payload: Payload) {
        Rc::get_mut(&mut (self.req).0).unwrap().payload = payload;
    }

    #[doc(hidden)]
    /// Set new app data container
    pub fn set_data_container(&mut self, extensions: Rc<Extensions>) {
        Rc::get_mut(&mut (self.req).0).unwrap().app_data = extensions;
    }

    /// Request extensions
    #[inline]
    pub fn extensions(&self) -> Ref<'_, Extensions> {
        self.req.extensions()
    }

    /// Mutable reference to a the request's extensions
    #[inline]
    pub fn extensions_mut(&self) -> RefMut<'_, Extensions> {
        self.req.extensions_mut()
    }
}

impl<Err> Resource<Uri> for WebRequest<Err> {
    fn path(&self) -> &str {
        self.match_info().path()
    }

    fn resource_path(&mut self) -> &mut Path<Uri> {
        self.match_info_mut()
    }
}

impl<Err> HttpMessage for WebRequest<Err> {
    #[inline]
    /// Returns Request's headers.
    fn message_headers(&self) -> &HeaderMap {
        &self.head().headers
    }

    /// Request extensions
    #[inline]
    fn message_extensions(&self) -> Ref<'_, Extensions> {
        self.req.extensions()
    }

    /// Mutable reference to a the request's extensions
    #[inline]
    fn message_extensions_mut(&self) -> RefMut<'_, Extensions> {
        self.req.extensions_mut()
    }
}

impl<Err: ErrorRenderer> fmt::Debug for WebRequest<Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "\nWebRequest {:?} {}:{}",
            self.head().version,
            self.head().method,
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

pub struct WebResponse<B = Body> {
    request: HttpRequest,
    response: Response<B>,
}

impl<B> WebResponse<B> {
    /// Create web response instance
    pub fn new(request: HttpRequest, response: Response<B>) -> Self {
        WebResponse { request, response }
    }

    /// Create web response from the error
    pub fn from_err<Err: ErrorRenderer, E: Into<Err::Container>>(
        err: E,
        request: HttpRequest,
    ) -> Self {
        use crate::http::error::ResponseError;

        let err = err.into();
        let res: Response = err.error_response();

        if res.head().status == StatusCode::INTERNAL_SERVER_ERROR {
            log::error!("Internal Server Error: {:?}", err);
        } else {
            log::debug!("Error in response: {:?}", err);
        }

        WebResponse {
            request,
            response: res.into_body(),
        }
    }

    /// Create web response for error
    #[inline]
    pub fn error_response<Err: ErrorRenderer, E: Into<Err::Container>>(
        self,
        err: E,
    ) -> Self {
        Self::from_err::<Err, E>(err, self.request)
    }

    /// Create web response
    #[inline]
    pub fn into_response<B1>(self, response: Response<B1>) -> WebResponse<B1> {
        WebResponse::new(self.request, response)
    }

    /// Get reference to original request
    #[inline]
    pub fn request(&self) -> &HttpRequest {
        &self.request
    }

    /// Get reference to response
    #[inline]
    pub fn response(&self) -> &Response<B> {
        &self.response
    }

    /// Get mutable reference to response
    #[inline]
    pub fn response_mut(&mut self) -> &mut Response<B> {
        &mut self.response
    }

    /// Get the response status code
    #[inline]
    pub fn status(&self) -> StatusCode {
        self.response.status()
    }

    #[inline]
    /// Returns response's headers.
    pub fn headers(&self) -> &HeaderMap {
        self.response.headers()
    }

    #[inline]
    /// Returns mutable response's headers.
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        self.response.headers_mut()
    }

    /// Execute closure and in case of error convert it to response.
    pub fn checked_expr<F, E, Err>(mut self, f: F) -> Self
    where
        F: FnOnce(&mut Self) -> Result<(), E>,
        E: Into<Err::Container>,
        Err: ErrorRenderer,
    {
        match f(&mut self) {
            Ok(_) => self,
            Err(err) => {
                let res: Response = err.into().into();
                WebResponse::new(self.request, res.into_body())
            }
        }
    }

    /// Extract response body
    pub fn take_body(&mut self) -> ResponseBody<B> {
        self.response.take_body()
    }
}

impl<B> WebResponse<B> {
    /// Set a new body
    pub fn map_body<F, B2>(self, f: F) -> WebResponse<B2>
    where
        F: FnOnce(&mut ResponseHead, ResponseBody<B>) -> ResponseBody<B2>,
    {
        let response = self.response.map_body(f);

        WebResponse {
            response,
            request: self.request,
        }
    }
}

impl<B> Into<Response<B>> for WebResponse<B> {
    fn into(self) -> Response<B> {
        self.response
    }
}

impl<B: MessageBody> fmt::Debug for WebResponse<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let res = writeln!(
            f,
            "\nWebResponse {:?} {}{}",
            self.response.head().version,
            self.response.head().status,
            self.response.head().reason.unwrap_or(""),
        );
        let _ = writeln!(f, "  headers:");
        for (key, val) in self.response.head().headers.iter() {
            let _ = writeln!(f, "    {:?}: {:?}", key, val);
        }
        let _ = writeln!(f, "  body: {:?}", self.response.body().size());
        res
    }
}

pub struct WebService {
    rdef: Vec<String>,
    name: Option<String>,
    guards: Vec<Box<dyn Guard>>,
}

impl WebService {
    /// Create new `WebService` instance.
    pub fn new<T: IntoPattern>(path: T) -> Self {
        WebService {
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
    pub fn finish<T, F, Err>(self, service: F) -> impl HttpServiceFactory<Err>
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

impl<T, Err> HttpServiceFactory<Err> for WebServiceImpl<T>
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
    fn register(mut self, config: &mut AppService<Err>) {
        let guards = if self.guards.is_empty() {
            None
        } else {
            Some(std::mem::replace(&mut self.guards, Vec::new()))
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

#[cfg(test)]
mod tests {
    use futures::future::ok;

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

    #[ntex_rt::test]
    async fn test_service() {
        let mut srv = init_service(App::new().service(
            web::service("/test").name("test").finish(
                |req: WebRequest<DefaultError>| {
                    ok(req.into_response(HttpResponse::Ok().finish()))
                },
            ),
        ))
        .await;
        let req = TestRequest::with_uri("/test").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let mut srv = init_service(App::new().service(
            web::service("/test").guard(guard::Get()).finish(
                |req: WebRequest<DefaultError>| {
                    ok(req.into_response(HttpResponse::Ok().finish()))
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
