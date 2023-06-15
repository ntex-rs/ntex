use std::{cell::RefCell, io, rc::Rc};

use ntex_h2::{self as h2, client::Client, frame};

use crate::http::body::{BodySize, MessageBody};
use crate::http::header::{self, HeaderMap, HeaderValue};
use crate::http::message::{RequestHeadType, ResponseHead};
use crate::http::{h2::payload, payload::Payload, Method, Version};
use crate::util::{poll_fn, ByteString, Bytes, HashMap, Ready};
use crate::{channel::oneshot, service::Ctx, service::Service};

use super::error::SendRequestError;

pub(super) async fn send_request<B>(
    mut client: H2Client,
    head: RequestHeadType,
    body: B,
) -> Result<(ResponseHead, Payload), SendRequestError>
where
    B: MessageBody,
{
    trace!("Sending client request: {:?} {:?}", head, body.size());
    let length = body.size();
    let eof = if head.as_ref().method == Method::HEAD {
        true
    } else {
        matches!(
            length,
            BodySize::None | BodySize::Empty | BodySize::Sized(0)
        )
    };

    // Extracting extra headers from RequestHeadType. HeaderMap::new() does not allocate.
    let (head, extra_headers) = match head {
        RequestHeadType::Owned(head) => (RequestHeadType::Owned(head), HeaderMap::new()),
        RequestHeadType::Rc(head, extra_headers) => (
            RequestHeadType::Rc(head, None),
            extra_headers.unwrap_or_else(HeaderMap::new),
        ),
    };

    // merging headers from head and extra headers.
    let mut hdrs: HeaderMap = head
        .as_ref()
        .headers
        .iter()
        .filter(|(name, _)| {
            !matches!(*name, &header::CONNECTION | &header::TRANSFER_ENCODING)
                || !extra_headers.contains_key(*name)
        })
        .chain(extra_headers.iter())
        .collect();

    // Content length
    match length {
        BodySize::None | BodySize::Stream => (),
        BodySize::Empty => {
            hdrs.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"))
        }
        BodySize::Sized(len) => hdrs.insert(
            header::CONTENT_LENGTH,
            HeaderValue::try_from(format!("{}", len)).unwrap(),
        ),
    };

    // send request
    let uri = &head.as_ref().uri;
    let path = uri
        .path_and_query()
        .map(|p| ByteString::from(format!("{}", p)))
        .unwrap_or_else(|| ByteString::from(uri.path()));
    let stream = client
        .inner
        .client
        .send_request(head.as_ref().method.clone(), path, hdrs, eof)
        .await?;

    // send body
    let id = stream.id();
    if eof {
        let result = client.wait_response(id).await;
        client.set_stream(stream);
        result
    } else {
        // sending body is async process, we can handle upload and download
        // at the same time
        let c = client.clone();
        crate::rt::spawn(async move {
            if let Err(e) = send_body(body, &stream).await {
                c.set_error(stream.id(), e);
            } else {
                c.set_stream(stream);
            }
        });
        client.wait_response(id).await
    }
}

async fn send_body<B: MessageBody>(
    mut body: B,
    stream: &h2::Stream,
) -> Result<(), SendRequestError> {
    loop {
        match poll_fn(|cx| body.poll_next_chunk(cx)).await {
            Some(Ok(b)) => {
                log::debug!("{:?} sending chunk, {} bytes", stream.id(), b.len());
                stream.send_payload(b, false).await?
            }
            Some(Err(e)) => return Err(e.into()),
            None => {
                log::debug!("{:?} eof of send stream ", stream.id());
                stream.send_payload(Bytes::new(), true).await?;
                return Ok(());
            }
        }
    }
}

pub(super) struct H2Client {
    inner: Rc<H2ClientInner>,
    wait_id: Option<frame::StreamId>,
}

impl Clone for H2Client {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            wait_id: None,
        }
    }
}

impl H2Client {
    pub(super) fn new(client: Client) -> Self {
        Self {
            wait_id: None,
            inner: Rc::new(H2ClientInner {
                client,
                streams: RefCell::new(HashMap::default()),
            }),
        }
    }

    pub(super) fn close(&self) {
        self.inner.client.close()
    }

    pub(super) fn is_closed(&self) -> bool {
        self.inner.client.is_closed()
    }

    fn set_error(&self, id: frame::StreamId, err: SendRequestError) {
        if let Some(mut info) = self.inner.streams.borrow_mut().remove(&id) {
            if let Some(tx) = info.tx.take() {
                let _ = tx.send(Err(err));
            }
        }
    }

    fn set_stream(&self, stream: h2::Stream) {
        if let Some(info) = self.inner.streams.borrow_mut().get_mut(&stream.id()) {
            // response is not received yet
            if info.tx.is_some() {
                info.stream = Some(stream);
            } else if let Some(ref mut sender) = info.payload {
                sender.set_stream(Some(stream));
            }
        }
    }

    async fn wait_response(
        &mut self,
        id: frame::StreamId,
    ) -> Result<(ResponseHead, Payload), SendRequestError> {
        let (tx, rx) = oneshot::channel();
        let info = StreamInfo {
            tx: Some(tx),
            stream: None,
            payload: None,
        };
        self.wait_id = Some(id);
        self.inner.streams.borrow_mut().insert(id, info);

        let result = match rx.await {
            Ok(item) => item,
            Err(_) => Err(SendRequestError::Error(Box::new(io::Error::new(
                io::ErrorKind::Other,
                "disconnected",
            )))),
        };
        self.wait_id = None;
        result
    }
}

impl Drop for H2Client {
    fn drop(&mut self) {
        if let Some(id) = self.wait_id.take() {
            self.inner.streams.borrow_mut().remove(&id);
        }
    }
}

struct H2ClientInner {
    client: Client,
    streams: RefCell<HashMap<frame::StreamId, StreamInfo>>,
}

#[derive(Debug)]
struct StreamInfo {
    tx: Option<oneshot::Sender<Result<(ResponseHead, Payload), SendRequestError>>>,
    stream: Option<h2::Stream>,
    payload: Option<payload::PayloadSender>,
}

pub(super) struct H2PublishService(Rc<H2ClientInner>);

impl H2PublishService {
    pub(super) fn new(client: H2Client) -> Self {
        Self(client.inner.clone())
    }
}

impl Service<h2::Message> for H2PublishService {
    type Response = ();
    type Error = &'static str;
    type Future<'f> = Ready<Self::Response, Self::Error>;

    fn call<'a>(&'a self, mut msg: h2::Message, _: Ctx<'a, Self>) -> Self::Future<'a> {
        match msg.kind().take() {
            h2::MessageKind::Headers {
                pseudo,
                headers,
                eof,
            } => {
                log::trace!(
                    "{:?} got response (eof: {}): {:#?}\nheaders: {:#?}",
                    msg.id(),
                    eof,
                    pseudo,
                    headers
                );

                let status = match pseudo.status {
                    Some(status) => status,
                    None => {
                        if let Some(mut info) =
                            self.0.streams.borrow_mut().remove(&msg.id())
                        {
                            let _ = info.tx.take().unwrap().send(Err(
                                SendRequestError::H2(h2::OperationError::Connection(
                                    h2::ConnectionError::MissingPseudo("Status"),
                                )),
                            ));
                        }
                        return Ready::Err("Missing status header");
                    }
                };

                let mut head = ResponseHead::new(status);
                head.headers = headers;
                head.version = Version::HTTP_2;

                if let Some(info) = self.0.streams.borrow_mut().get_mut(&msg.id()) {
                    let stream = info.stream.take();
                    let payload = if !eof {
                        log::debug!("Creating local payload stream for {:?}", msg.id());
                        let (sender, payload) =
                            payload::Payload::create(msg.stream().empty_capacity());
                        sender.set_stream(stream);
                        info.payload = Some(sender);
                        Payload::H2(payload)
                    } else {
                        Payload::None
                    };
                    let _ = info.tx.take().unwrap().send(Ok((head, payload)));
                    Ready::Ok(())
                } else {
                    Ready::Err("Cannot find Stream info")
                }
            }
            h2::MessageKind::Data(data, cap) => {
                log::debug!("Got data chunk for {:?}: {:?}", msg.id(), data.len());
                if let Some(info) = self.0.streams.borrow_mut().get_mut(&msg.id()) {
                    if let Some(ref mut pl) = info.payload {
                        pl.feed_data(data, cap);
                    }
                    Ready::Ok(())
                } else {
                    log::error!("Payload stream does not exists for {:?}", msg.id());
                    Ready::Err("Cannot find Stream info")
                }
            }
            h2::MessageKind::Eof(item) => {
                log::debug!("Got payload eof for {:?}: {:?}", msg.id(), item);
                if let Some(mut info) = self.0.streams.borrow_mut().remove(&msg.id()) {
                    if let Some(ref mut pl) = info.payload {
                        match item {
                            h2::StreamEof::Data(data) => {
                                pl.feed_eof(data);
                            }
                            h2::StreamEof::Trailers(_) => {
                                pl.feed_eof(Bytes::new());
                            }
                            h2::StreamEof::Error(err) => pl.set_error(err.into()),
                        }
                    }
                    Ready::Ok(())
                } else {
                    Ready::Err("Cannot find Stream info")
                }
            }
            h2::MessageKind::Disconnect(err) => {
                log::debug!("Connection is disconnected {:?}", err);
                if let Some(mut info) = self.0.streams.borrow_mut().remove(&msg.id()) {
                    if let Some(ref mut pl) = info.payload {
                        pl.set_error(io::Error::new(io::ErrorKind::Other, err).into());
                    }
                }
                Ready::Ok(())
            }
            h2::MessageKind::Empty => Ready::Ok(()),
        }
    }
}
