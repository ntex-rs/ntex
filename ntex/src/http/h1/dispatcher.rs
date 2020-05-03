use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::{fmt, io, mem, net};

use bitflags::bitflags;
use bytes::{Buf, BytesMut};
use futures::ready;
use pin_project::{pin_project, project};

use crate::codec::{AsyncRead, AsyncWrite, Decoder, Encoder, Framed, FramedParts};
use crate::http::body::{Body, BodySize, MessageBody, ResponseBody};
use crate::http::config::DispatcherConfig;
use crate::http::error::{DispatchError, PayloadError, ResponseError};
use crate::http::helpers::DataFactory;
use crate::http::request::Request;
use crate::http::response::Response;
use crate::rt::time::{delay_until, Delay, Instant};
use crate::Service;

use super::codec::Codec;
use super::payload::{Payload, PayloadSender, PayloadStatus};
use super::{Message, MessageType, MAX_BUFFER_SIZE};

const READ_LW_BUFFER_SIZE: usize = 1024;
const READ_HW_BUFFER_SIZE: usize = 4096;
const WRITE_LW_BUFFER_SIZE: usize = 2048;
const WRITE_HW_BUFFER_SIZE: usize = 8192;
const BUFFER_SIZE: usize = 32_768;
const LW_PIPELINED_MESSAGES: usize = 1;

bitflags! {
    pub struct Flags: u8 {
        /// We parsed one complete request message
        const STARTED            = 0b0000_0001;
        /// Keep-alive is enabled on current connection
        const KEEPALIVE          = 0b0000_0010;
        /// Socket is disconnected, read or write side
        const DISCONNECT         = 0b0000_0100;
        /// Connection is upgraded or request parse error (bad request)
        const STOP_READING       = 0b0000_1000;
        /// Shutdown is in process (flushing and io shutdown timer)
        const SHUTDOWN           = 0b0001_0000;
        /// Io shutdown process started
        const SHUTDOWN_IO        = 0b0010_0000;
        /// Shutdown timer is started
        const SHUTDOWN_TM        = 0b0100_0000;
        /// Connection is upgraded
        const UPGRADE            = 0b1000_0000;
    }
}

/// Dispatcher for HTTP/1.1 protocol
#[pin_project]
pub struct Dispatcher<T, S, B, X, U>
where
    S: Service<Request = Request>,
    S::Error: ResponseError,
    B: MessageBody,
    X: Service<Request = Request, Response = Request>,
    X::Error: ResponseError,
    U: Service<Request = (Request, Framed<T, Codec>), Response = ()>,
    U::Error: fmt::Display,
{
    #[pin]
    call: CallState<S, X>,
    inner: InnerDispatcher<T, S, B, X, U>,
    #[pin]
    upgrade: Option<U::Future>,
}

#[pin_project]
enum CallState<S: Service, X: Service> {
    Io,
    Expect(#[pin] X::Future),
    Service(#[pin] S::Future),
}

impl<S: Service, X: Service> CallState<S, X> {
    fn is_io(&self) -> bool {
        match self {
            CallState::Io => true,
            _ => false,
        }
    }
}

struct InnerDispatcher<T, S, B, X, U>
where
    S: Service<Request = Request>,
    S::Error: ResponseError,
    B: MessageBody,
    X: Service<Request = Request, Response = Request>,
    X::Error: ResponseError,
    U: Service<Request = (Request, Framed<T, Codec>), Response = ()>,
    U::Error: fmt::Display,
{
    config: Rc<DispatcherConfig<S, X, U>>,
    on_connect: Option<Box<dyn DataFactory>>,
    peer_addr: Option<net::SocketAddr>,
    flags: Flags,
    error: Option<DispatchError>,

    send_payload: Option<ResponseBody<B>>,
    payload: Option<PayloadSender>,
    messages: VecDeque<DispatcherMessage>,

    ka_expire: Instant,
    ka_timer: Option<Delay>,

    io: Option<T>,
    read_buf: BytesMut,
    write_buf: BytesMut,
    codec: Codec,
}

enum DispatcherMessage {
    Request(Request),
    Upgrade(Request),
    Error(Response<()>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PollWrite {
    /// allowed to process next request
    AllowNext,
    /// write buffer is full
    Pending,
    /// waiting for response stream (app response)
    /// or write buffer is full
    PendingRespnse,
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum PollRead {
    NoUpdates,
    HasUpdates,
}

enum CallProcess<S: Service, X: Service, U: Service> {
    /// next call is available
    Next(CallState<S, X>),
    /// waiting for service call response completion
    Pending,
    /// call queue is empty
    Io,
    /// Upgrade connection
    Upgrade(U::Future),
}

impl<T, S, B, X, U> Dispatcher<T, S, B, X, U>
where
    T: AsyncRead + AsyncWrite + Unpin,
    S: Service<Request = Request>,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    X: Service<Request = Request, Response = Request>,
    X::Error: ResponseError,
    U: Service<Request = (Request, Framed<T, Codec>), Response = ()>,
    U::Error: fmt::Display,
{
    /// Create http/1 dispatcher.
    pub(in crate::http) fn new(
        config: Rc<DispatcherConfig<S, X, U>>,
        stream: T,
        peer_addr: Option<net::SocketAddr>,
        on_connect: Option<Box<dyn DataFactory>>,
    ) -> Self {
        let codec = Codec::new(config.timer.clone(), config.keep_alive_enabled());
        // slow request timer
        let timeout = config.client_timer();

        Dispatcher::with_timeout(
            config,
            stream,
            codec,
            BytesMut::with_capacity(READ_HW_BUFFER_SIZE),
            timeout,
            peer_addr,
            on_connect,
        )
    }

    /// Create http/1 dispatcher with slow request timeout.
    pub(in crate::http) fn with_timeout(
        config: Rc<DispatcherConfig<S, X, U>>,
        io: T,
        codec: Codec,
        read_buf: BytesMut,
        timeout: Option<Delay>,
        peer_addr: Option<net::SocketAddr>,
        on_connect: Option<Box<dyn DataFactory>>,
    ) -> Self {
        let keepalive = config.keep_alive_enabled();
        let flags = if keepalive {
            Flags::KEEPALIVE
        } else {
            Flags::empty()
        };

        // keep-alive timer
        let (ka_expire, ka_timer) = if let Some(delay) = timeout {
            (delay.deadline(), Some(delay))
        } else if let Some(delay) = config.keep_alive_timer() {
            (delay.deadline(), Some(delay))
        } else {
            (config.now(), None)
        };

        Dispatcher {
            call: CallState::Io,
            upgrade: None,
            inner: InnerDispatcher {
                write_buf: BytesMut::with_capacity(WRITE_HW_BUFFER_SIZE),
                payload: None,
                send_payload: None,
                error: None,
                messages: VecDeque::new(),
                io: Some(io),
                config,
                codec,
                read_buf,
                flags,
                peer_addr,
                on_connect,
                ka_expire,
                ka_timer,
            },
        }
    }
}

impl<T, S, B, X, U> Future for Dispatcher<T, S, B, X, U>
where
    T: AsyncRead + AsyncWrite + Unpin,
    S: Service<Request = Request>,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    X: Service<Request = Request, Response = Request>,
    X::Error: ResponseError,
    U: Service<Request = (Request, Framed<T, Codec>), Response = ()>,
    U::Error: fmt::Display,
{
    type Output = Result<(), DispatchError>;

    #[project]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        // upgrade
        if this.inner.flags.contains(Flags::UPGRADE) {
            return this.upgrade.as_pin_mut().unwrap().poll(cx).map_err(|e| {
                error!("Upgrade handler error: {}", e);
                DispatchError::Upgrade
            });
        }

        // keep-alive book-keeping
        this.inner.poll_keepalive(cx, this.call.is_io())?;

        // shutdown process
        if this.inner.flags.contains(Flags::SHUTDOWN) {
            return this.inner.poll_shutdown(cx);
        }

        loop {
            // process incoming stream
            this.inner.poll_read(cx)?;

            #[project]
            let st = match this.call.project() {
                CallState::Service(mut fut) => loop {
                    // we have to loop because of
                    // read back-pressure, check Poll::Pending processing
                    match fut.poll(cx) {
                        Poll::Ready(result) => match result {
                            Ok(res) => break this.inner.process_response(res.into())?,
                            Err(e) => {
                                let res: Response = e.into();
                                break this.inner.process_response(
                                    res.map_body(|_, body| body.into_body()),
                                )?;
                            }
                        },
                        Poll::Pending => {
                            // if read-backpressure is enabled, we might need
                            // to read more data (ie service future can wait for payload data)
                            if this.inner.payload.is_some()
                                && this.inner.poll_read(cx)? == PollRead::HasUpdates
                            {
                                // poll_request has read more data, try
                                // to poll service future again

                                // restore consumed future
                                this = self.as_mut().project();
                                #[project]
                                let new_fut = match this.call.project() {
                                    CallState::Service(fut) => fut,
                                    _ => panic!(),
                                };
                                fut = new_fut;
                                continue;
                            }
                            break CallProcess::Pending;
                        }
                    }
                },
                // handle EXPECT call
                CallState::Expect(fut) => match fut.poll(cx) {
                    Poll::Ready(result) => match result {
                        Ok(req) => {
                            this.inner
                                .write_buf
                                .extend_from_slice(b"HTTP/1.1 100 Continue\r\n\r\n");
                            CallProcess::Next(CallState::Service(
                                this.inner.config.service.call(req),
                            ))
                        }
                        Err(e) => {
                            let res: Response = e.into();
                            this.inner.process_response(
                                res.map_body(|_, body| body.into_body()),
                            )?
                        }
                    },
                    Poll::Pending => {
                        // expect service call must resolve before
                        // we can do any more io processing.
                        //
                        // TODO: check keep-alive timer interaction
                        return Poll::Pending;
                    }
                },
                CallState::Io => CallProcess::Io,
            };

            let processing = match st {
                CallProcess::Next(st) => {
                    // we have next call state, just proceed with it
                    this = self.as_mut().project();
                    this.call.set(st);
                    continue;
                }
                CallProcess::Pending => {
                    // service response is in process,
                    // we just flush output and that is it
                    this.inner.poll_write(cx)?;
                    true
                }
                CallProcess::Io => {
                    // service call queue is empty, we can process next request
                    match this.inner.poll_write(cx)? {
                        PollWrite::AllowNext => {
                            match this.inner.process_messages(CallProcess::Io)? {
                                CallProcess::Next(st) => {
                                    this = self.as_mut().project();
                                    this.call.set(st);
                                    continue;
                                }
                                CallProcess::Upgrade(fut) => {
                                    this.upgrade.set(Some(fut));
                                    return self.poll(cx);
                                }
                                CallProcess::Io => false,
                                CallProcess::Pending => unreachable!(),
                            }
                        }
                        PollWrite::Pending => false,
                        PollWrite::PendingRespnse => {
                            !this.inner.flags.contains(Flags::DISCONNECT)
                        }
                    }
                }
                CallProcess::Upgrade(fut) => {
                    this.upgrade.set(Some(fut));
                    return self.poll(cx);
                }
            };

            // socket is closed and we are not processing any service responses
            if this
                .inner
                .flags
                .intersects(Flags::DISCONNECT | Flags::STOP_READING)
                && !processing
            {
                trace!("Shutdown connection (no more work) {:?}", this.inner.flags);
                this.inner.flags.insert(Flags::SHUTDOWN);
            }
            // we dont have any parsed requests and output buffer is flushed
            else if !processing && this.inner.write_buf.is_empty() {
                if let Some(err) = this.inner.error.take() {
                    trace!("Dispatcher error {:?}", err);
                    return Poll::Ready(Err(err));
                }

                // disconnect if keep-alive is not enabled
                if this.inner.flags.contains(Flags::STARTED)
                    && !this.inner.flags.contains(Flags::KEEPALIVE)
                {
                    trace!("Shutdown, keep-alive is not enabled");
                    this.inner.flags.insert(Flags::SHUTDOWN);
                }
            }

            // disconnect if shutdown
            return if this.inner.flags.contains(Flags::SHUTDOWN) {
                this.inner.poll_shutdown(cx)
            } else {
                Poll::Pending
            };
        }
    }
}

impl<T, S, B, X, U> InnerDispatcher<T, S, B, X, U>
where
    T: AsyncRead + AsyncWrite + Unpin,
    S: Service<Request = Request>,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    X: Service<Request = Request, Response = Request>,
    X::Error: ResponseError,
    U: Service<Request = (Request, Framed<T, Codec>), Response = ()>,
    U::Error: fmt::Display,
{
    /// shutdown process
    fn poll_shutdown(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), DispatchError>> {
        // we can not do anything here
        if self.flags.contains(Flags::DISCONNECT) {
            return Poll::Ready(Ok(()));
        }

        if !self.flags.contains(Flags::SHUTDOWN_IO) {
            self.poll_flush(cx)?;

            if self.write_buf.is_empty() {
                ready!(Pin::new(self.io.as_mut().unwrap()).poll_shutdown(cx)?);
                self.flags.insert(Flags::SHUTDOWN_IO);
            }
        }

        // read until 0 or err
        let mut buf = [0u8; 512];
        while let Poll::Ready(res) =
            Pin::new(self.io.as_mut().unwrap()).poll_read(cx, &mut buf)
        {
            match res {
                Err(_) | Ok(0) => return Poll::Ready(Ok(())),
                _ => (),
            }
        }

        // shutdown timeout
        if self.ka_timer.is_none() {
            if self.flags.contains(Flags::SHUTDOWN_TM) {
                // shutdown timeout is not enabled
                Poll::Pending
            } else {
                self.flags.insert(Flags::SHUTDOWN_TM);
                if let Some(interval) = self.config.client_disconnect_timer() {
                    trace!("Start shutdown timer for {:?}", interval);
                    self.ka_timer = Some(delay_until(interval));
                    let _ = Pin::new(&mut self.ka_timer.as_mut().unwrap()).poll(cx);
                }
                Poll::Pending
            }
        } else {
            let mut timer = self.ka_timer.as_mut().unwrap();

            // configure timer
            if !self.flags.contains(Flags::SHUTDOWN_TM) {
                if let Some(interval) = self.config.client_disconnect_timer() {
                    self.flags.insert(Flags::SHUTDOWN_TM);
                    timer.reset(interval);
                } else {
                    let _ = self.ka_timer.take();
                    return Poll::Pending;
                }
            }

            match Pin::new(&mut timer).poll(cx) {
                Poll::Ready(_) => {
                    // if we get timeout during shutdown, drop connection
                    Poll::Ready(Err(DispatchError::DisconnectTimeout))
                }
                _ => Poll::Pending,
            }
        }
    }

    /// Flush stream
    fn poll_flush(&mut self, cx: &mut Context<'_>) -> Result<(), DispatchError> {
        let len = self.write_buf.len();
        if len == 0 {
            return Ok(());
        }

        let mut written = 0;
        while written < len {
            match Pin::new(self.io.as_mut().unwrap())
                .poll_write(cx, &self.write_buf[written..])
            {
                Poll::Ready(Ok(n)) => {
                    if n == 0 {
                        trace!("Disconnected during flush, written {}", written);
                        return Err(DispatchError::Io(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "failed to write frame to transport",
                        )));
                    } else {
                        written += n
                    }
                }
                Poll::Pending => break,
                Poll::Ready(Err(e)) => {
                    trace!("Error during flush: {}", e);
                    return Err(DispatchError::Io(e));
                }
            }
        }
        if written == len {
            // flushed same amount as buffer, we dont need to reallocate
            unsafe { self.write_buf.set_len(0) }
        } else {
            self.write_buf.advance(written);
        }
        Ok(())
    }

    fn send_response(
        &mut self,
        msg: Response<()>,
        body: ResponseBody<B>,
    ) -> Result<bool, DispatchError> {
        trace!("Sending response: {:?} body: {:?}", msg, body.size());
        // we dont need to process responses if socket is disconnected
        // but we still want to handle requests with app service
        if !self.flags.contains(Flags::DISCONNECT) {
            self.codec
                .encode(Message::Item((msg, body.size())), &mut self.write_buf)
                .map_err(|err| {
                    if let Some(mut payload) = self.payload.take() {
                        payload.set_error(PayloadError::Incomplete(None));
                    }
                    DispatchError::Io(err)
                })?;

            self.flags.set(Flags::KEEPALIVE, self.codec.keepalive());

            match body.size() {
                BodySize::None | BodySize::Empty => Ok(true),
                _ => {
                    self.send_payload = Some(body);
                    Ok(false)
                }
            }
        } else {
            Ok(false)
        }
    }

    fn poll_write(&mut self, cx: &mut Context<'_>) -> Result<PollWrite, DispatchError> {
        let mut flushed = false;

        while let Some(ref mut stream) = self.send_payload {
            let len = self.write_buf.len();

            if len < BUFFER_SIZE {
                // increase write buffer
                let remaining = self.write_buf.capacity() - len;
                if remaining < WRITE_LW_BUFFER_SIZE {
                    self.write_buf.reserve(BUFFER_SIZE - remaining);
                }

                match stream.poll_next_chunk(cx) {
                    Poll::Ready(Some(Ok(item))) => {
                        trace!("Got response chunk: {:?}", item.len());
                        flushed = false;
                        self.codec
                            .encode(Message::Chunk(Some(item)), &mut self.write_buf)?;
                    }
                    Poll::Ready(None) => {
                        trace!("Response payload eof");
                        flushed = false;
                        self.codec
                            .encode(Message::Chunk(None), &mut self.write_buf)?;
                        self.send_payload = None;
                        break;
                    }
                    Poll::Ready(Some(Err(e))) => {
                        trace!("Error during response body poll: {:?}", e);
                        return Err(DispatchError::Unknown);
                    }
                    Poll::Pending => {
                        // response payload stream is not ready
                        // we can only flush
                        if !flushed {
                            self.poll_flush(cx)?;
                        }
                        return Ok(PollWrite::PendingRespnse);
                    }
                }
            } else {
                // write buffer is full, try to flush and check if we have
                // space in buffer
                flushed = true;
                self.poll_flush(cx)?;
                if self.write_buf.len() >= BUFFER_SIZE {
                    return Ok(PollWrite::PendingRespnse);
                }
            }
        }

        if !flushed {
            self.poll_flush(cx)?;
        }

        // we have enought space in write bffer
        if self.write_buf.len() < BUFFER_SIZE {
            Ok(PollWrite::AllowNext)
        } else {
            Ok(PollWrite::Pending)
        }
    }

    /// Process one incoming requests
    fn poll_read(&mut self, cx: &mut Context<'_>) -> Result<PollRead, DispatchError> {
        // read socket data into a buf
        if !self
            .flags
            .intersects(Flags::DISCONNECT | Flags::STOP_READING)
        {
            // limit amount of non processed requests
            // drain messages queue until it contains just 1 message
            // or request payload is consumed and requires more data (backpressure off)
            if self.messages.len() > LW_PIPELINED_MESSAGES
                || !self
                    .payload
                    .as_ref()
                    .map(|info| info.need_read(cx) == PayloadStatus::Read)
                    .unwrap_or(true)
            {
                return Ok(PollRead::NoUpdates);
            }

            // read data from socket
            let io = self.io.as_mut().unwrap();
            let buf = &mut self.read_buf;
            let mut updated = false;
            while buf.len() < MAX_BUFFER_SIZE {
                // increase read buffer size
                let remaining = buf.capacity() - buf.len();
                if remaining < READ_LW_BUFFER_SIZE {
                    buf.reserve(BUFFER_SIZE);
                }

                match read(cx, io, buf) {
                    Poll::Pending => break,
                    Poll::Ready(Ok(n)) => {
                        if n == 0 {
                            trace!(
                                "Disconnected during read, buffer size {}",
                                buf.len()
                            );
                            self.flags.insert(Flags::DISCONNECT);
                            break;
                        } else {
                            updated = true;
                        }
                    }
                    Poll::Ready(Err(e)) => {
                        trace!("Error during read: {:?}", e);
                        self.flags.insert(Flags::DISCONNECT);
                        self.error = Some(DispatchError::Io(e));
                        break;
                    }
                }
            }

            if !updated {
                return Ok(PollRead::NoUpdates);
            }
        }

        if self.read_buf.is_empty() {
            Ok(PollRead::NoUpdates)
        } else {
            let result = self.input_decode();

            // socket is disconnected clear read buf
            if self.flags.contains(Flags::DISCONNECT) {
                self.read_buf.clear();
                if let Some(mut payload) = self.payload.take() {
                    payload.feed_eof();
                }
            }
            result
        }
    }

    fn internal_error(&mut self, msg: &'static str) {
        error!("{}", msg);
        self.flags.insert(Flags::DISCONNECT);
        self.messages.push_back(DispatcherMessage::Error(
            Response::InternalServerError().finish().drop_body(),
        ));
        self.error = Some(DispatchError::InternalError);
    }

    fn input_decode(&mut self) -> Result<PollRead, DispatchError> {
        let mut updated = false;
        loop {
            match self.codec.decode(&mut self.read_buf) {
                Ok(Some(msg)) => {
                    updated = true;
                    self.flags.insert(Flags::STARTED);

                    match msg {
                        Message::Item(mut req) => {
                            let pl = self.codec.message_type();
                            req.head_mut().peer_addr = self.peer_addr;

                            // set on_connect data
                            if let Some(ref on_connect) = self.on_connect {
                                on_connect.set(&mut req.extensions_mut());
                            }

                            // handle upgrade request
                            if pl == MessageType::Stream && self.config.upgrade.is_some()
                            {
                                self.flags.insert(Flags::STOP_READING);
                                self.messages.push_back(DispatcherMessage::Upgrade(req));
                                break;
                            }

                            // handle request with payload
                            if pl == MessageType::Payload || pl == MessageType::Stream {
                                let (ps, pl) = Payload::create(false);
                                let (req1, _) =
                                    req.replace_payload(crate::http::Payload::H1(pl));
                                req = req1;
                                self.payload = Some(ps);
                            }

                            self.messages.push_back(DispatcherMessage::Request(req));
                        }
                        Message::Chunk(Some(chunk)) => {
                            if let Some(ref mut payload) = self.payload {
                                payload.feed_data(chunk);
                            } else {
                                self.internal_error(
                                    "Internal server error: unexpected payload chunk",
                                );
                                break;
                            }
                        }
                        Message::Chunk(None) => {
                            if let Some(mut payload) = self.payload.take() {
                                payload.feed_eof();
                            } else {
                                self.internal_error(
                                    "Internal server error: unexpected eof",
                                );
                                break;
                            }
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    // error during request decoding
                    if let Some(mut payload) = self.payload.take() {
                        payload.set_error(PayloadError::EncodingCorrupted);
                    }

                    // Malformed requests should be responded with 400
                    self.messages.push_back(DispatcherMessage::Error(
                        Response::BadRequest().finish().drop_body(),
                    ));
                    self.flags.insert(Flags::STOP_READING);
                    self.read_buf.clear();
                    self.error = Some(e.into());
                    break;
                }
            }
        }

        if updated && self.ka_timer.is_some() {
            if let Some(expire) = self.config.keep_alive_expire() {
                self.ka_expire = expire;
            }
        }

        if updated {
            Ok(PollRead::HasUpdates)
        } else {
            Ok(PollRead::NoUpdates)
        }
    }

    /// keep-alive timer
    fn poll_keepalive(
        &mut self,
        cx: &mut Context<'_>,
        is_empty: bool,
    ) -> Result<(), DispatchError> {
        // do nothing for disconnected or upgrade socket or if keep-alive timer is disabled
        if self.flags.intersects(Flags::DISCONNECT | Flags::UPGRADE)
            || self.ka_timer.is_none()
        {
            return Ok(());
        }

        if !self.flags.contains(Flags::STARTED) {
            // slow request timeout
            if Pin::new(&mut self.ka_timer.as_mut().unwrap())
                .poll(cx)
                .is_ready()
            {
                // timeout on first request (slow request) return 408
                trace!("Slow request timeout");
                let _ = self.send_response(
                    Response::RequestTimeout().finish().drop_body(),
                    ResponseBody::Other(Body::Empty),
                );
                self.flags.insert(Flags::STARTED | Flags::SHUTDOWN);
            }
        } else {
            let mut timer = self.ka_timer.as_mut().unwrap();

            // keep-alive timer
            if Pin::new(&mut timer).poll(cx).is_ready() {
                if timer.deadline() >= self.ka_expire {
                    // check for any outstanding tasks
                    if is_empty && self.write_buf.is_empty() {
                        trace!("Keep-alive timeout, close connection");
                        self.flags.insert(Flags::SHUTDOWN);
                        return Ok(());
                    } else if let Some(dl) = self.config.keep_alive_expire() {
                        // extend keep-alive timer
                        timer.reset(dl);
                    }
                } else {
                    timer.reset(self.ka_expire);
                }
                let _ = Pin::new(&mut timer).poll(cx);
            }
        }
        Ok(())
    }

    fn process_response(
        &mut self,
        res: Response<B>,
    ) -> Result<CallProcess<S, X, U>, DispatchError> {
        let (res, body) = res.replace_body(());
        if self.send_response(res, body)? {
            // response does not have body, so we can process next request
            self.process_messages(CallProcess::Next(CallState::Io))
        } else {
            Ok(CallProcess::Next(CallState::Io))
        }
    }

    fn process_messages(
        &mut self,
        io: CallProcess<S, X, U>,
    ) -> Result<CallProcess<S, X, U>, DispatchError> {
        while let Some(msg) = self.messages.pop_front() {
            return match msg {
                DispatcherMessage::Request(req) => {
                    // Handle `EXPECT: 100-Continue` header
                    Ok(CallProcess::Next(if req.head().expect() {
                        CallState::Expect(self.config.expect.call(req))
                    } else {
                        CallState::Service(self.config.service.call(req))
                    }))
                }
                // switch to upgrade handler
                DispatcherMessage::Upgrade(req) => {
                    self.flags.insert(Flags::UPGRADE);
                    let mut parts = FramedParts::with_read_buf(
                        self.io.take().unwrap(),
                        mem::take(&mut self.codec),
                        mem::take(&mut self.read_buf),
                    );
                    parts.write_buf = mem::take(&mut self.write_buf);
                    let framed = Framed::from_parts(parts);

                    Ok(CallProcess::Upgrade(
                        self.config.upgrade.as_ref().unwrap().call((req, framed)),
                    ))
                }
                DispatcherMessage::Error(res) => {
                    if self.send_response(res, ResponseBody::Other(Body::Empty))? {
                        // response does not have body, so we can process next request
                        continue;
                    } else {
                        return Ok(io);
                    }
                }
            };
        }
        Ok(io)
    }
}

fn read<T>(
    cx: &mut Context<'_>,
    io: &mut T,
    buf: &mut BytesMut,
) -> Poll<Result<usize, io::Error>>
where
    T: AsyncRead + Unpin,
{
    Pin::new(io).poll_read_buf(cx, buf)
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use futures::future::{lazy, ok, Future, FutureExt};
    use futures::StreamExt;
    use rand::Rng;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::http::config::{DispatcherConfig, ServiceConfig};
    use crate::http::h1::{ClientCodec, ExpectHandler, UpgradeHandler};
    use crate::http::{body, Request, ResponseHead, StatusCode};
    use crate::rt::time::delay_for;
    use crate::service::IntoService;
    use crate::testing::Io;

    /// Create http/1 dispatcher.
    pub(crate) fn h1<F, S, B>(
        stream: Io,
        service: F,
    ) -> Dispatcher<Io, S, B, ExpectHandler, UpgradeHandler<Io>>
    where
        F: IntoService<S>,
        S: Service<Request = Request>,
        S::Error: ResponseError,
        S::Response: Into<Response<B>>,
        B: MessageBody,
    {
        Dispatcher::new(
            Rc::new(DispatcherConfig::new(
                ServiceConfig::default(),
                service.into_service(),
                ExpectHandler,
                None,
            )),
            stream,
            None,
            None,
        )
    }

    pub(crate) fn spawn_h1<F, S, B>(stream: Io, service: F)
    where
        F: IntoService<S>,
        S: Service<Request = Request> + 'static,
        S::Error: ResponseError,
        S::Response: Into<Response<B>>,
        B: MessageBody + 'static,
    {
        crate::rt::spawn(
            Dispatcher::<Io, S, B, ExpectHandler, UpgradeHandler<Io>>::new(
                Rc::new(DispatcherConfig::new(
                    ServiceConfig::default(),
                    service.into_service(),
                    ExpectHandler,
                    None,
                )),
                stream,
                None,
                None,
            ),
        );
    }

    fn load(decoder: &mut ClientCodec, buf: &mut BytesMut) -> ResponseHead {
        decoder.decode(buf).unwrap().unwrap()
    }

    #[ntex_rt::test]
    async fn test_req_parse_err() {
        let (client, server) = Io::create();
        client.remote_buffer_cap(1024);
        client.write("GET /test HTTP/1\r\n\r\n");

        let mut h1 = h1(server, |_| ok::<_, io::Error>(Response::Ok().finish()));
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert!(h1.inner.flags.contains(Flags::SHUTDOWN));
        client
            .local_buffer(|buf| assert_eq!(&buf[..26], b"HTTP/1.1 400 Bad Request\r\n"));

        client.close().await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_ready());
        assert!(h1.inner.flags.contains(Flags::SHUTDOWN_IO));
    }

    #[ntex_rt::test]
    async fn test_pipeline() {
        let (client, server) = Io::create();
        client.remote_buffer_cap(4096);
        let mut decoder = ClientCodec::default();
        spawn_h1(server, |_| ok::<_, io::Error>(Response::Ok().finish()));

        client.write("GET /test HTTP/1.1\r\n\r\n");

        let mut buf = client.read().await.unwrap();
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(!client.is_server_dropped());

        client.write("GET /test HTTP/1.1\r\n\r\n");
        client.write("GET /test HTTP/1.1\r\n\r\n");

        let mut buf = client.read().await.unwrap();
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(decoder.decode(&mut buf).unwrap().is_none());
        assert!(!client.is_server_dropped());

        client.close().await;
        assert!(client.is_server_dropped());
    }

    #[ntex_rt::test]
    async fn test_pipeline_with_delay() {
        let (client, server) = Io::create();
        client.remote_buffer_cap(4096);
        let mut decoder = ClientCodec::default();
        spawn_h1(server, |_| async {
            delay_for(Duration::from_millis(100)).await;
            Ok::<_, io::Error>(Response::Ok().finish())
        });

        client.write("GET /test HTTP/1.1\r\n\r\n");

        let mut buf = client.read().await.unwrap();
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(!client.is_server_dropped());

        client.write("GET /test HTTP/1.1\r\n\r\n");
        client.write("GET /test HTTP/1.1\r\n\r\n");
        delay_for(Duration::from_millis(50)).await;
        client.write("GET /test HTTP/1.1\r\n\r\n");

        let mut buf = client.read().await.unwrap();
        assert!(load(&mut decoder, &mut buf).status.is_success());

        let mut buf = client.read().await.unwrap();
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(decoder.decode(&mut buf).unwrap().is_none());
        assert!(!client.is_server_dropped());

        buf.extend(client.read().await.unwrap());
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(decoder.decode(&mut buf).unwrap().is_none());
        assert!(!client.is_server_dropped());

        client.close().await;
        assert!(client.is_server_dropped());
    }

    #[ntex_rt::test]
    /// if socket is disconnected
    /// h1 dispatcher still processes all incoming requests
    /// but it does not write any data to socket
    async fn test_write_disconnected() {
        let num = Arc::new(AtomicUsize::new(0));
        let num2 = num.clone();

        let (client, server) = Io::create();
        spawn_h1(server, move |_| {
            num2.fetch_add(1, Ordering::Relaxed);
            ok::<_, io::Error>(Response::Ok().finish())
        });

        client.remote_buffer_cap(1024);
        client.write("GET /test HTTP/1.1\r\n\r\n");
        client.write("GET /test HTTP/1.1\r\n\r\n");
        client.write("GET /test HTTP/1.1\r\n\r\n");
        client.close().await;
        assert!(client.is_server_dropped());
        assert!(client.read_any().is_empty());

        // all request must be handled
        assert_eq!(num.load(Ordering::Relaxed), 3);
    }

    #[ntex_rt::test]
    async fn test_read_large_message() {
        let (client, server) = Io::create();
        client.remote_buffer_cap(4096);

        let mut h1 = h1(server, |_| ok::<_, io::Error>(Response::Ok().finish()));
        let mut decoder = ClientCodec::default();

        let data = rand::thread_rng()
            .sample_iter(&rand::distributions::Alphanumeric)
            .take(70_000)
            .collect::<String>();
        client.write("GET /test HTTP/1.1\r\nContent-Length: ");
        client.write(data);

        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert!(h1.inner.flags.contains(Flags::SHUTDOWN));

        let mut buf = client.read().await.unwrap();
        assert_eq!(load(&mut decoder, &mut buf).status, StatusCode::BAD_REQUEST);
    }

    #[ntex_rt::test]
    async fn test_read_backpressure() {
        let mark = Arc::new(AtomicBool::new(false));
        let mark2 = mark.clone();

        let (client, server) = Io::create();
        client.remote_buffer_cap(4096);
        spawn_h1(server, move |mut req: Request| {
            let m = mark2.clone();
            async move {
                // read one chunk
                let mut pl = req.take_payload();
                let _ = pl.next().await.unwrap().unwrap();
                m.store(true, Ordering::Relaxed);
                // sleep
                delay_for(Duration::from_secs(999_999)).await;
                Ok::<_, io::Error>(Response::Ok().finish())
            }
        });

        client.write("GET /test HTTP/1.1\r\nContent-Length: 1048576\r\n\r\n");
        delay_for(Duration::from_millis(50)).await;

        // buf must be consumed
        assert_eq!(client.remote_buffer(|buf| buf.len()), 0);

        // io should be drained only by no more than MAX_BUFFER_SIZE
        let random_bytes: Vec<u8> =
            (0..1_048_576).map(|_| rand::random::<u8>()).collect();
        client.write(random_bytes);

        delay_for(Duration::from_millis(50)).await;
        assert!(client.remote_buffer(|buf| buf.len()) > 1_048_576 - MAX_BUFFER_SIZE * 3);
        assert!(mark.load(Ordering::Relaxed));
    }

    #[ntex_rt::test]
    async fn test_write_backpressure() {
        let num = Arc::new(AtomicUsize::new(0));
        let num2 = num.clone();

        struct Stream(Arc<AtomicUsize>);

        impl body::MessageBody for Stream {
            fn size(&self) -> body::BodySize {
                body::BodySize::Stream
            }
            fn poll_next_chunk(
                &mut self,
                _: &mut Context<'_>,
            ) -> Poll<Option<Result<Bytes, Box<dyn std::error::Error>>>> {
                let data = rand::thread_rng()
                    .sample_iter(&rand::distributions::Alphanumeric)
                    .take(65_536)
                    .collect::<String>();
                self.0.fetch_add(data.len(), Ordering::Relaxed);

                Poll::Ready(Some(Ok(Bytes::from(data))))
            }
        }

        let (client, server) = Io::create();
        let mut h1 = h1(server, move |_| {
            let n = num2.clone();
            async move { Ok::<_, io::Error>(Response::Ok().message_body(Stream(n.clone()))) }
            .boxed_local()
        });

        // do not allow to write to socket
        client.remote_buffer_cap(0);
        client.write("GET /test HTTP/1.1\r\n\r\n");
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());

        // buf must be consumed
        assert_eq!(client.remote_buffer(|buf| buf.len()), 0);

        // amount of generated data
        assert_eq!(num.load(Ordering::Relaxed), 65_536);

        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert_eq!(num.load(Ordering::Relaxed), 65_536);
        // response message + chunking encoding
        assert_eq!(h1.inner.write_buf.len(), 65629);

        client.remote_buffer_cap(65536);
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());
        assert_eq!(num.load(Ordering::Relaxed), 65_536 * 2);
    }

    #[ntex_rt::test]
    async fn test_disconnect_during_response_body_pending() {
        struct Stream(bool);

        impl body::MessageBody for Stream {
            fn size(&self) -> body::BodySize {
                body::BodySize::Sized(2048)
            }
            fn poll_next_chunk(
                &mut self,
                _: &mut Context<'_>,
            ) -> Poll<Option<Result<Bytes, Box<dyn std::error::Error>>>> {
                if self.0 {
                    Poll::Pending
                } else {
                    self.0 = true;
                    let data = rand::thread_rng()
                        .sample_iter(&rand::distributions::Alphanumeric)
                        .take(1024)
                        .collect::<String>();
                    Poll::Ready(Some(Ok(Bytes::from(data))))
                }
            }
        }

        let (client, server) = Io::create();
        client.remote_buffer_cap(4096);
        let mut h1 = h1(server, |_| {
            ok::<_, io::Error>(Response::Ok().message_body(Stream(false)))
        });

        client.write("GET /test HTTP/1.1\r\n\r\n");
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());

        // buf must be consumed
        assert_eq!(client.remote_buffer(|buf| buf.len()), 0);

        let mut decoder = ClientCodec::default();
        let mut buf = client.read().await.unwrap();
        assert!(load(&mut decoder, &mut buf).status.is_success());
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_pending());

        client.close().await;
        assert!(lazy(|cx| Pin::new(&mut h1).poll(cx)).await.is_ready());
    }
}
