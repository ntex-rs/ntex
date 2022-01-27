use std::convert::TryFrom;

use h2::SendStream;
use http::header::{HeaderValue, CONNECTION, CONTENT_LENGTH, TRANSFER_ENCODING};
use http::{request::Request, Method, Version};

use crate::http::body::{BodySize, MessageBody};
use crate::http::header::HeaderMap;
use crate::http::message::{RequestHeadType, ResponseHead};
use crate::http::payload::Payload;
use crate::util::{poll_fn, Bytes};

use super::connection::H2Sender;
use super::error::SendRequestError;

pub(super) async fn send_request<B>(
    io: H2Sender,
    head: RequestHeadType,
    body: B,
) -> Result<(ResponseHead, Payload), SendRequestError>
where
    B: MessageBody,
{
    trace!("Sending client request: {:?} {:?}", head, body.size());
    let head_req = head.as_ref().method == Method::HEAD;
    let length = body.size();
    let eof = matches!(
        length,
        BodySize::None | BodySize::Empty | BodySize::Sized(0)
    );

    let mut req = Request::new(());
    *req.uri_mut() = head.as_ref().uri.clone();
    *req.method_mut() = head.as_ref().method.clone();
    *req.version_mut() = Version::HTTP_2;

    let mut skip_len = true;

    // Content length
    let _ = match length {
        BodySize::None => None,
        BodySize::Stream => {
            skip_len = false;
            None
        }
        BodySize::Empty => req
            .headers_mut()
            .insert(CONTENT_LENGTH, HeaderValue::from_static("0")),
        BodySize::Sized(len) => req.headers_mut().insert(
            CONTENT_LENGTH,
            HeaderValue::try_from(format!("{}", len)).unwrap(),
        ),
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
    let headers = head
        .as_ref()
        .headers
        .iter()
        .filter(|(name, _)| !extra_headers.contains_key(*name))
        .chain(extra_headers.iter());

    // copy headers
    for (key, value) in headers {
        match *key {
            CONNECTION | TRANSFER_ENCODING => continue, // http2 specific
            CONTENT_LENGTH if skip_len => continue,
            _ => (),
        }
        req.headers_mut().append(key, value.clone());
    }

    let mut sender = io.get_sender();
    let res = poll_fn(|cx| sender.poll_ready(cx)).await;
    if let Err(e) = res {
        log::trace!("SendRequest readiness failed: {:?}", e);
        return Err(SendRequestError::from(e));
    }

    let resp = match sender.send_request(req, eof) {
        Ok((fut, send)) => {
            if !eof {
                send_body(body, send).await?;
            }
            fut.await.map_err(SendRequestError::from)?
        }
        Err(e) => {
            return Err(e.into());
        }
    };

    let (parts, body) = resp.into_parts();
    let payload = if head_req { Payload::None } else { body.into() };

    let mut head = ResponseHead::new(parts.status);
    head.version = parts.version;
    head.headers = parts.headers.into();
    Ok((head, payload))
}

async fn send_body<B: MessageBody>(
    mut body: B,
    mut send: SendStream<Bytes>,
) -> Result<(), SendRequestError> {
    let mut buf = None;
    loop {
        if buf.is_none() {
            match poll_fn(|cx| body.poll_next_chunk(cx)).await {
                Some(Ok(b)) => {
                    send.reserve_capacity(b.len());
                    buf = Some(b);
                }
                Some(Err(e)) => return Err(e.into()),
                None => {
                    if let Err(e) = send.send_data(Bytes::new(), true) {
                        return Err(e.into());
                    }
                    send.reserve_capacity(0);
                    return Ok(());
                }
            }
        }

        match poll_fn(|cx| send.poll_capacity(cx)).await {
            None => return Ok(()),
            Some(Ok(cap)) => {
                let b = buf.as_mut().unwrap();
                let len = b.len();
                let bytes = b.split_to(std::cmp::min(cap, len));

                if let Err(e) = send.send_data(bytes, false) {
                    return Err(e.into());
                } else {
                    if !b.is_empty() {
                        send.reserve_capacity(b.len());
                    } else {
                        buf = None;
                    }
                    continue;
                }
            }
            Some(Err(e)) => return Err(e.into()),
        }
    }
}
