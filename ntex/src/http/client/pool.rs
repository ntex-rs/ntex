use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use std::{cell::RefCell, collections::VecDeque, future::Future, pin::Pin, rc::Rc};

use h2::client::{handshake, Connection, SendRequest};
use http::uri::Authority;

use crate::channel::pool;
use crate::codec::{AsyncRead, AsyncWrite, ReadBuf};
use crate::http::Protocol;
use crate::rt::{spawn, time::sleep, time::Sleep};
use crate::service::Service;
use crate::task::LocalWaker;
use crate::util::{poll_fn, Bytes, HashMap};

use super::connection::{ConnectionType, IoConnection};
use super::error::ConnectError;
use super::Connect;

#[derive(Hash, Eq, PartialEq, Clone, Debug)]
pub(super) struct Key {
    authority: Authority,
}

impl From<Authority> for Key {
    fn from(authority: Authority) -> Key {
        Key { authority }
    }
}

type Waiter<Io> = pool::Sender<Result<IoConnection<Io>, ConnectError>>;
type WaiterReceiver<Io> = pool::Receiver<Result<IoConnection<Io>, ConnectError>>;
const ZERO: Duration = Duration::from_millis(0);

/// Connections pool
pub(super) struct ConnectionPool<T, Io: 'static>(Rc<T>, Rc<RefCell<Inner<Io>>>);

impl<T, Io> ConnectionPool<T, Io>
where
    Io: AsyncRead + AsyncWrite + Unpin + 'static,
    T: Service<Request = Connect, Response = (Io, Protocol), Error = ConnectError>
        + Unpin
        + 'static,
    T::Future: Unpin,
{
    pub(super) fn new(
        connector: T,
        conn_lifetime: Duration,
        conn_keep_alive: Duration,
        disconnect_timeout: Duration,
        limit: usize,
    ) -> Self {
        let connector = Rc::new(connector);
        let inner = Rc::new(RefCell::new(Inner {
            conn_lifetime,
            conn_keep_alive,
            disconnect_timeout,
            limit,
            acquired: 0,
            waiters: VecDeque::new(),
            available: HashMap::default(),
            pool: pool::new(),
            waker: LocalWaker::new(),
        }));

        // start pool support future
        crate::rt::spawn(ConnectionPoolSupport {
            connector: connector.clone(),
            inner: inner.clone(),
        });

        ConnectionPool(connector, inner)
    }
}

impl<T, Io> Drop for ConnectionPool<T, Io>
where
    Io: 'static,
{
    fn drop(&mut self) {
        self.1.borrow().waker.wake();
    }
}

impl<T, Io> Clone for ConnectionPool<T, Io>
where
    Io: 'static,
{
    fn clone(&self) -> Self {
        ConnectionPool(self.0.clone(), self.1.clone())
    }
}

impl<T, Io> Service for ConnectionPool<T, Io>
where
    Io: AsyncRead + AsyncWrite + Unpin + 'static,
    T: Service<Request = Connect, Response = (Io, Protocol), Error = ConnectError>
        + 'static,
    T::Future: Unpin,
{
    type Request = Connect;
    type Response = IoConnection<Io>;
    type Error = ConnectError;
    type Future = Pin<Box<dyn Future<Output = Result<IoConnection<Io>, ConnectError>>>>;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        self.0.poll_shutdown(cx, is_error)
    }

    #[inline]
    fn call(&self, req: Connect) -> Self::Future {
        trace!("Request connection to {:?}", req.uri);
        let connector = self.0.clone();
        let inner = self.1.clone();

        Box::pin(async move {
            let key = if let Some(authority) = req.uri.authority() {
                authority.clone().into()
            } else {
                return Err(ConnectError::Unresolved);
            };

            // acquire connection
            match poll_fn(|cx| Poll::Ready(inner.borrow_mut().acquire(&key, cx))).await {
                // use existing connection
                Acquire::Acquired(io, created) => {
                    trace!("Use existing connection for {:?}", req.uri);
                    Ok(IoConnection::new(
                        io,
                        created,
                        Some(Acquired(key, Some(inner))),
                    ))
                }
                // open new tcp connection
                Acquire::Available => {
                    trace!("Connecting to {:?}", req.uri);
                    let (tx, rx) = inner.borrow_mut().pool.channel();
                    OpenConnection::spawn(key, tx, inner, connector.call(req));

                    match rx.await {
                        Err(_) => Err(ConnectError::Disconnected),
                        Ok(res) => res,
                    }
                }
                // pool is full, wait
                Acquire::NotAvailable => {
                    trace!(
                        "Pool is full, waiting for available connections for {:?}",
                        req.uri
                    );
                    let rx = inner.borrow_mut().wait_for(req);
                    match rx.await {
                        Err(_) => Err(ConnectError::Disconnected),
                        Ok(res) => res,
                    }
                }
            }
        })
    }
}

enum Acquire<T> {
    Acquired(ConnectionType<T>, Instant),
    Available,
    NotAvailable,
}

struct AvailableConnection<Io> {
    io: ConnectionType<Io>,
    used: Instant,
    created: Instant,
}

pub(super) struct Inner<Io> {
    conn_lifetime: Duration,
    conn_keep_alive: Duration,
    disconnect_timeout: Duration,
    limit: usize,
    acquired: usize,
    available: HashMap<Key, VecDeque<AvailableConnection<Io>>>,
    waiters: VecDeque<(Key, Connect, Waiter<Io>)>,
    waker: LocalWaker,
    pool: pool::Pool<Result<IoConnection<Io>, ConnectError>>,
}

impl<Io> Inner<Io> {
    fn reserve(&mut self) {
        self.acquired += 1;
    }

    fn release(&mut self) {
        self.acquired -= 1;
    }
}

impl<Io> Inner<Io>
where
    Io: AsyncRead + AsyncWrite + Unpin + 'static,
{
    /// connection is not available, wait
    fn wait_for(&mut self, connect: Connect) -> WaiterReceiver<Io> {
        let (tx, rx) = self.pool.channel();
        let key: Key = connect.uri.authority().unwrap().clone().into();
        self.waiters.push_back((key, connect, tx));

        rx
    }

    /// cleanup dropped waiters
    fn cleanup(&mut self) {
        // cleanup waiters
        while !self.waiters.is_empty() {
            let (_, _, tx) = self.waiters.front().unwrap();
            // check if waiter is still alive
            if tx.is_canceled() {
                self.waiters.pop_front();
                continue;
            };
            break;
        }
    }

    fn acquire(&mut self, key: &Key, cx: &mut Context<'_>) -> Acquire<Io> {
        self.cleanup();

        // check limits
        if self.limit > 0 && self.acquired >= self.limit {
            return Acquire::NotAvailable;
        }

        self.reserve();

        // check if open connection is available
        // cleanup stale connections at the same time
        if let Some(ref mut connections) = self.available.get_mut(key) {
            let now = Instant::now();
            while let Some(conn) = connections.pop_back() {
                // check if it still usable
                if (now - conn.used) > self.conn_keep_alive
                    || (now - conn.created) > self.conn_lifetime
                {
                    if let ConnectionType::H1(io) = conn.io {
                        CloseConnection::spawn(io, self.disconnect_timeout);
                    }
                } else {
                    let mut io = conn.io;
                    let mut buf = [0; 2];
                    let mut read_buf = ReadBuf::new(&mut buf);
                    if let ConnectionType::H1(ref mut s) = io {
                        match Pin::new(s).poll_read(cx, &mut read_buf) {
                            Poll::Pending => (),
                            Poll::Ready(Ok(_)) if !read_buf.filled().is_empty() => {
                                if let ConnectionType::H1(io) = io {
                                    CloseConnection::spawn(io, self.disconnect_timeout);
                                }
                                continue;
                            }
                            _ => continue,
                        }
                    }
                    return Acquire::Acquired(io, conn.created);
                }
            }
        }
        Acquire::Available
    }

    fn release_conn(&mut self, key: &Key, io: ConnectionType<Io>, created: Instant) {
        self.acquired -= 1;
        self.available
            .entry(key.clone())
            .or_insert_with(VecDeque::new)
            .push_back(AvailableConnection {
                io,
                created,
                used: Instant::now(),
            });
        self.check_availibility();
    }

    fn release_close(&mut self, io: ConnectionType<Io>) {
        self.acquired -= 1;
        if let ConnectionType::H1(io) = io {
            CloseConnection::spawn(io, self.disconnect_timeout);
        }
        self.check_availibility();
    }

    fn check_availibility(&mut self) {
        self.cleanup();
        if !self.waiters.is_empty() && self.acquired < self.limit {
            self.waker.wake();
        }
    }
}

struct ConnectionPoolSupport<T, Io>
where
    Io: AsyncRead + AsyncWrite + Unpin + 'static,
{
    connector: T,
    inner: Rc<RefCell<Inner<Io>>>,
}

impl<T, Io> Future for ConnectionPoolSupport<T, Io>
where
    Io: AsyncRead + AsyncWrite + Unpin + 'static,
    T: Service<Request = Connect, Response = (Io, Protocol), Error = ConnectError>
        + Unpin,
    T::Future: Unpin + 'static,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // we are last copy
        if Rc::strong_count(&this.inner) == 1 {
            return Poll::Ready(());
        }

        let mut inner = this.inner.as_ref().borrow_mut();
        inner.waker.register(cx.waker());

        // check waiters
        while let Some((key, _, tx)) = inner.waiters.front() {
            // is waiter still alive
            if tx.is_canceled() {
                inner.waiters.pop_front();
                continue;
            };
            let key = key.clone();

            match inner.acquire(&key, cx) {
                Acquire::NotAvailable => break,
                Acquire::Acquired(io, created) => {
                    let (key, _, tx) = inner.waiters.pop_front().unwrap();
                    let _ = tx.send(Ok(IoConnection::new(
                        io,
                        created,
                        Some(Acquired(key.clone(), Some(this.inner.clone()))),
                    )));
                }
                Acquire::Available => {
                    let (key, connect, tx) = inner.waiters.pop_front().unwrap();
                    OpenConnection::spawn(
                        key,
                        tx,
                        this.inner.clone(),
                        this.connector.call(connect),
                    );
                }
            }
        }

        Poll::Pending
    }
}

pin_project_lite::pin_project! {
    struct CloseConnection<T> {
        io: T,
        #[pin]
        timeout: Option<Sleep>,
        shutdown: bool,
    }
}

impl<T> CloseConnection<T>
where
    T: AsyncWrite + AsyncRead + Unpin + 'static,
{
    fn spawn(io: T, timeout: Duration) {
        let timeout = if timeout != ZERO {
            Some(sleep(timeout))
        } else {
            None
        };
        spawn(Self {
            io,
            timeout,
            shutdown: false,
        });
    }
}

impl<T> Future for CloseConnection<T>
where
    T: AsyncWrite + AsyncRead + Unpin,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut this = self.project();

        // shutdown WRITE side
        match Pin::new(&mut this.io).poll_shutdown(cx) {
            Poll::Ready(Ok(())) => *this.shutdown = true,
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(_)) => return Poll::Ready(()),
        }

        // read until 0 or err
        if let Some(timeout) = this.timeout.as_pin_mut() {
            match timeout.poll(cx) {
                Poll::Ready(_) => (),
                Poll::Pending => {
                    let mut buf = [0u8; 512];
                    let mut read_buf = ReadBuf::new(&mut buf);
                    loop {
                        match Pin::new(&mut this.io).poll_read(cx, &mut read_buf) {
                            Poll::Pending => return Poll::Pending,
                            Poll::Ready(Err(_)) => return Poll::Ready(()),
                            Poll::Ready(Ok(_)) => {
                                if read_buf.filled().is_empty() {
                                    return Poll::Ready(());
                                }
                                continue;
                            }
                        }
                    }
                }
            }
        }
        Poll::Ready(())
    }
}

struct OpenConnection<F, Io>
where
    Io: AsyncRead + AsyncWrite + Unpin + 'static,
{
    fut: F,
    h2: Option<
        Pin<
            Box<
                dyn Future<
                    Output = Result<
                        (SendRequest<Bytes>, Connection<Io, Bytes>),
                        h2::Error,
                    >,
                >,
            >,
        >,
    >,
    tx: Option<Waiter<Io>>,
    guard: Option<OpenGuard<Io>>,
}

impl<F, Io> OpenConnection<F, Io>
where
    F: Future<Output = Result<(Io, Protocol), ConnectError>> + Unpin + 'static,
    Io: AsyncRead + AsyncWrite + Unpin + 'static,
{
    fn spawn(key: Key, tx: Waiter<Io>, inner: Rc<RefCell<Inner<Io>>>, fut: F) {
        spawn(OpenConnection {
            fut,
            h2: None,
            tx: Some(tx),
            guard: Some(OpenGuard {
                key,
                inner: Some(inner),
            }),
        });
    }
}

impl<F, Io> Future for OpenConnection<F, Io>
where
    F: Future<Output = Result<(Io, Protocol), ConnectError>> + Unpin,
    Io: AsyncRead + AsyncWrite + Unpin,
{
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().get_mut();

        // handle http2 connection
        if let Some(ref mut h2) = this.h2 {
            return match Pin::new(h2).poll(cx) {
                Poll::Ready(Ok((snd, connection))) => {
                    // h2 connection is ready
                    let conn = IoConnection::new(
                        ConnectionType::H2(snd),
                        Instant::now(),
                        Some(this.guard.take().unwrap().consume()),
                    );
                    if let Err(Ok(conn)) = this.tx.take().unwrap().send(Ok(conn)) {
                        // waiter is gone, return connection to pool
                        conn.release()
                    }
                    spawn(async move {
                        let _ = connection.await;
                    });
                    Poll::Ready(())
                }
                Poll::Pending => Poll::Pending,
                Poll::Ready(Err(err)) => {
                    if let Some(rx) = this.tx.take() {
                        let _ = rx.send(Err(ConnectError::H2(err)));
                    }
                    Poll::Ready(())
                }
            };
        }

        // open tcp connection
        match Pin::new(&mut this.fut).poll(cx) {
            Poll::Ready(Err(err)) => {
                trace!("Failed to open client connection {:?}", err);
                if let Some(rx) = this.tx.take() {
                    let _ = rx.send(Err(err));
                }
                Poll::Ready(())
            }
            Poll::Ready(Ok((io, proto))) => {
                trace!("Connection is established");
                // handle http1 proto
                if proto == Protocol::Http1 {
                    let conn = IoConnection::new(
                        ConnectionType::H1(io),
                        Instant::now(),
                        Some(this.guard.take().unwrap().consume()),
                    );
                    if let Err(Ok(conn)) = this.tx.take().unwrap().send(Ok(conn)) {
                        // waiter is gone, return connection to pool
                        conn.release()
                    }
                    Poll::Ready(())
                } else {
                    // init http2 handshake
                    this.h2 = Some(Box::pin(handshake(io)));
                    self.poll(cx)
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

struct OpenGuard<Io>
where
    Io: AsyncRead + AsyncWrite + Unpin + 'static,
{
    key: Key,
    inner: Option<Rc<RefCell<Inner<Io>>>>,
}

impl<Io> OpenGuard<Io>
where
    Io: AsyncRead + AsyncWrite + Unpin + 'static,
{
    fn consume(mut self) -> Acquired<Io> {
        Acquired(self.key.clone(), self.inner.take())
    }
}

impl<Io> Drop for OpenGuard<Io>
where
    Io: AsyncRead + AsyncWrite + Unpin + 'static,
{
    fn drop(&mut self) {
        if let Some(i) = self.inner.take() {
            let mut inner = i.as_ref().borrow_mut();
            inner.release();
            inner.check_availibility();
        }
    }
}

pub(super) struct Acquired<T>(Key, Option<Rc<RefCell<Inner<T>>>>);

impl<T> Acquired<T>
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
{
    pub(super) fn close(&mut self, conn: IoConnection<T>) {
        if let Some(inner) = self.1.take() {
            let (io, _) = conn.into_inner();
            inner.as_ref().borrow_mut().release_close(io);
        }
    }

    pub(super) fn release(&mut self, conn: IoConnection<T>) {
        if let Some(inner) = self.1.take() {
            let (io, created) = conn.into_inner();
            inner
                .as_ref()
                .borrow_mut()
                .release_conn(&self.0, io, created);
        }
    }
}

impl<T> Drop for Acquired<T> {
    fn drop(&mut self) {
        if let Some(inner) = self.1.take() {
            inner.borrow_mut().release();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, convert::TryFrom, rc::Rc, time::Duration};

    use super::*;
    use crate::rt::time::sleep;
    use crate::{
        http::client::Connection, http::Uri, service::fn_service, testing::Io,
        util::lazy,
    };

    #[crate::rt_test]
    async fn test_basics() {
        let store = Rc::new(RefCell::new(Vec::new()));
        let store2 = store.clone();

        let pool = ConnectionPool::new(
            fn_service(move |req| {
                let (client, server) = Io::create();
                store2.borrow_mut().push((req, server));
                Box::pin(async move { Ok((client, Protocol::Http1)) })
            }),
            Duration::from_secs(10),
            Duration::from_secs(10),
            Duration::from_millis(0),
            1,
        )
        .clone();

        // uri must contain authority
        let req = Connect {
            uri: Uri::try_from("/test").unwrap(),
            addr: None,
        };
        match pool.call(req).await {
            Err(ConnectError::Unresolved) => (),
            _ => panic!(),
        }

        // connect one
        let req = Connect {
            uri: Uri::try_from("http://localhost/test").unwrap(),
            addr: None,
        };
        let conn = pool.call(req.clone()).await.unwrap();
        assert_eq!(store.borrow().len(), 1);
        assert!(format!("{:?}", conn).contains("H1Connection"));
        assert_eq!(conn.protocol(), Protocol::Http1);
        assert_eq!(pool.1.borrow().acquired, 1);

        // pool is full, waiting
        let mut fut = pool.call(req.clone());
        assert!(lazy(|cx| Pin::new(&mut fut).poll(cx)).await.is_pending());
        assert_eq!(pool.1.borrow().waiters.len(), 1);

        // release connection and push it to next waiter
        conn.release();
        let _conn = fut.await.unwrap();
        assert_eq!(store.borrow().len(), 1);
        assert!(pool.1.borrow().waiters.is_empty());

        // drop waiter, no interest in connection
        let mut fut = pool.call(req.clone());
        assert!(lazy(|cx| Pin::new(&mut fut).poll(cx)).await.is_pending());
        drop(fut);
        sleep(Duration::from_millis(50)).await;
        pool.1.borrow_mut().check_availibility();
        assert!(pool.1.borrow().waiters.is_empty());

        assert!(lazy(|cx| pool.poll_ready(cx)).await.is_ready());
        assert!(lazy(|cx| pool.poll_shutdown(cx, false)).await.is_ready());
    }
}
