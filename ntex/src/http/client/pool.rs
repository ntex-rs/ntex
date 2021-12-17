use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use std::{cell::RefCell, collections::VecDeque, future::Future, pin::Pin, rc::Rc};

use h2::client::{Builder, Connection as H2Connection, SendRequest};
use http::uri::Authority;
use ntex_tls::types::HttpProtocol;

use crate::channel::pool;
use crate::io::IoBoxed;
use crate::rt::spawn;
use crate::service::Service;
use crate::task::LocalWaker;
use crate::time::{now, Millis};
use crate::util::{Bytes, HashMap};

use super::connection::{Connection, ConnectionType};
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

type Waiter = pool::Sender<Result<Connection, ConnectError>>;
type WaiterReceiver = pool::Receiver<Result<Connection, ConnectError>>;

/// Connections pool
pub(super) struct ConnectionPool<T>(Rc<T>, Rc<RefCell<Inner>>);

impl<T> ConnectionPool<T>
where
    T: Service<Request = Connect, Response = IoBoxed, Error = ConnectError>
        + Unpin
        + 'static,
    T::Future: Unpin,
{
    pub(super) fn new(
        connector: T,
        conn_lifetime: Duration,
        conn_keep_alive: Duration,
        disconnect_timeout: Millis,
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

impl<T> Drop for ConnectionPool<T> {
    fn drop(&mut self) {
        self.1.borrow().waker.wake();
    }
}

impl<T> Clone for ConnectionPool<T> {
    fn clone(&self) -> Self {
        ConnectionPool(self.0.clone(), self.1.clone())
    }
}

impl<T> Service for ConnectionPool<T>
where
    T: Service<Request = Connect, Response = IoBoxed, Error = ConnectError> + 'static,
    T::Future: Unpin,
{
    type Request = Connect;
    type Response = Connection;
    type Error = ConnectError;
    type Future = Pin<Box<dyn Future<Output = Result<Connection, ConnectError>>>>;

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
            let result = inner.borrow_mut().acquire(&key);
            match result {
                // use existing connection
                Acquire::Acquired(io, created) => {
                    trace!("Use existing connection for {:?}", req.uri);
                    Ok(Connection::new(
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

enum Acquire {
    Acquired(ConnectionType, Instant),
    Available,
    NotAvailable,
}

struct AvailableConnection {
    io: ConnectionType,
    used: Instant,
    created: Instant,
}

pub(super) struct Inner {
    conn_lifetime: Duration,
    conn_keep_alive: Duration,
    disconnect_timeout: Millis,
    limit: usize,
    acquired: usize,
    available: HashMap<Key, VecDeque<AvailableConnection>>,
    waiters: VecDeque<(Key, Connect, Waiter)>,
    waker: LocalWaker,
    pool: pool::Pool<Result<Connection, ConnectError>>,
}

impl Inner {
    fn reserve(&mut self) {
        self.acquired += 1;
    }

    fn release(&mut self) {
        self.acquired -= 1;
    }
}

impl Inner {
    /// connection is not available, wait
    fn wait_for(&mut self, connect: Connect) -> WaiterReceiver {
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

    fn acquire(&mut self, key: &Key) -> Acquire {
        self.cleanup();

        // check limits
        if self.limit > 0 && self.acquired >= self.limit {
            return Acquire::NotAvailable;
        }

        self.reserve();

        // check if open connection is available
        // cleanup stale connections at the same time
        if let Some(ref mut connections) = self.available.get_mut(key) {
            let now = now();
            while let Some(conn) = connections.pop_back() {
                // check if it still usable
                if (now - conn.used) > self.conn_keep_alive
                    || (now - conn.created) > self.conn_lifetime
                {
                    if let ConnectionType::H1(io) = conn.io {
                        spawn(async move {
                            let _ = io.shutdown().await;
                        });
                    }
                } else {
                    let io = conn.io;
                    if let ConnectionType::H1(ref s) = io {
                        if s.is_closed() {
                            continue;
                        }
                        let is_valid = s.read().with_buf(|buf| {
                            if buf.is_empty() || (buf.len() == 2 && &buf[..] == b"\r\n")
                            {
                                buf.clear();
                                true
                            } else {
                                false
                            }
                        });
                        if !is_valid {
                            continue;
                        }
                    }
                    return Acquire::Acquired(io, conn.created);
                }
            }
        }
        Acquire::Available
    }

    fn release_conn(&mut self, key: &Key, io: ConnectionType, created: Instant) {
        self.acquired -= 1;
        self.available
            .entry(key.clone())
            .or_insert_with(VecDeque::new)
            .push_back(AvailableConnection {
                io,
                created,
                used: now(),
            });
        self.check_availibility();
    }

    fn release_close(&mut self, io: ConnectionType) {
        self.acquired -= 1;
        if let ConnectionType::H1(io) = io {
            spawn(async move {
                let _ = io.shutdown().await;
            });
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

struct ConnectionPoolSupport<T> {
    connector: T,
    inner: Rc<RefCell<Inner>>,
}

impl<T> Future for ConnectionPoolSupport<T>
where
    T: Service<Request = Connect, Response = IoBoxed, Error = ConnectError> + Unpin,
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

            match inner.acquire(&key) {
                Acquire::NotAvailable => break,
                Acquire::Acquired(io, created) => {
                    let (key, _, tx) = inner.waiters.pop_front().unwrap();
                    let _ = tx.send(Ok(Connection::new(
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

struct OpenConnection<F> {
    fut: F,
    h2: Option<
        Pin<
            Box<
                dyn Future<
                    Output = Result<
                        (SendRequest<Bytes>, H2Connection<IoBoxed, Bytes>),
                        h2::Error,
                    >,
                >,
            >,
        >,
    >,
    tx: Option<Waiter>,
    guard: Option<OpenGuard>,
    disconnect_timeout: Millis,
}

impl<F> OpenConnection<F>
where
    F: Future<Output = Result<IoBoxed, ConnectError>> + Unpin + 'static,
{
    fn spawn(key: Key, tx: Waiter, inner: Rc<RefCell<Inner>>, fut: F) {
        let disconnect_timeout = inner.borrow().disconnect_timeout;

        spawn(OpenConnection {
            fut,
            disconnect_timeout,
            h2: None,
            tx: Some(tx),
            guard: Some(OpenGuard {
                key,
                inner: Some(inner),
            }),
        });
    }
}

impl<F> Future for OpenConnection<F>
where
    F: Future<Output = Result<IoBoxed, ConnectError>> + Unpin,
{
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().get_mut();

        // handle http2 connection
        if let Some(ref mut h2) = this.h2 {
            return match Pin::new(h2).poll(cx) {
                Poll::Ready(Ok((snd, connection))) => {
                    // h2 connection is ready
                    let conn = Connection::new(
                        ConnectionType::H2(snd),
                        now(),
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
            Poll::Ready(Ok(io)) => {
                io.set_disconnect_timeout(this.disconnect_timeout);

                // handle http2 proto
                if io.query::<HttpProtocol>().get() == Some(HttpProtocol::Http2) {
                    log::trace!("Connection is established, start http2 handshake");
                    // init http2 handshake
                    this.h2 = Some(Box::pin(Builder::new().handshake(io)));
                    self.poll(cx)
                } else {
                    log::trace!("Connection is established, init http1 connection");
                    let conn = Connection::new(
                        ConnectionType::H1(io),
                        now(),
                        Some(this.guard.take().unwrap().consume()),
                    );
                    if let Err(Ok(conn)) = this.tx.take().unwrap().send(Ok(conn)) {
                        // waiter is gone, return connection to pool
                        conn.release()
                    }
                    Poll::Ready(())
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

struct OpenGuard {
    key: Key,
    inner: Option<Rc<RefCell<Inner>>>,
}

impl OpenGuard {
    fn consume(mut self) -> Acquired {
        Acquired(self.key.clone(), self.inner.take())
    }
}

impl Drop for OpenGuard {
    fn drop(&mut self) {
        if let Some(i) = self.inner.take() {
            let mut inner = i.as_ref().borrow_mut();
            inner.release();
            inner.check_availibility();
        }
    }
}

pub(super) struct Acquired(Key, Option<Rc<RefCell<Inner>>>);

impl Acquired {
    pub(super) fn close(&mut self, conn: Connection) {
        if let Some(inner) = self.1.take() {
            let (io, _) = conn.into_inner();
            inner.as_ref().borrow_mut().release_close(io);
        }
    }

    pub(super) fn release(&mut self, conn: Connection) {
        if let Some(inner) = self.1.take() {
            let (io, created) = conn.into_inner();
            inner
                .as_ref()
                .borrow_mut()
                .release_conn(&self.0, io, created);
        }
    }
}

impl Drop for Acquired {
    fn drop(&mut self) {
        if let Some(inner) = self.1.take() {
            inner.borrow_mut().release();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, convert::TryFrom, rc::Rc};

    use super::*;
    use crate::{
        http::Uri, io as nio, service::fn_service, testing::Io, time::sleep, util::lazy,
    };

    #[crate::rt_test]
    async fn test_basics() {
        let store = Rc::new(RefCell::new(Vec::new()));
        let store2 = store.clone();

        let pool = ConnectionPool::new(
            fn_service(move |req| {
                let (client, server) = Io::create();
                store2.borrow_mut().push((req, server));
                Box::pin(async move { Ok(nio::Io::new(client).into_boxed()) })
            }),
            Duration::from_secs(10),
            Duration::from_secs(10),
            Millis::ZERO,
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
        assert_eq!(conn.protocol(), HttpProtocol::Http1);
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
        sleep(Millis(50)).await;
        pool.1.borrow_mut().check_availibility();
        assert!(pool.1.borrow().waiters.is_empty());

        assert!(lazy(|cx| pool.poll_ready(cx)).await.is_ready());
        assert!(lazy(|cx| pool.poll_shutdown(cx, false)).await.is_ready());
    }
}
