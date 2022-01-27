use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use std::{cell::RefCell, collections::VecDeque, future::Future, pin::Pin, rc::Rc};

use h2::client::{Builder, Connection as H2Connection, SendRequest};
use http::uri::Authority;
use ntex_tls::types::HttpProtocol;

use crate::io::{IoBoxed, TokioIoBoxed};
use crate::time::{now, Millis};
use crate::util::{ready, Bytes, HashMap, HashSet};
use crate::{channel::pool, rt::spawn, service::Service, task::LocalWaker};

use super::connection::{Connection, ConnectionType, H2Sender};
use super::{error::ConnectError, Connect};

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

enum Acquire {
    Acquired(ConnectionType, Instant),
    Available,
    NotAvailable,
}

#[derive(Debug)]
struct AvailableConnection {
    io: ConnectionType,
    used: Instant,
    created: Instant,
}

/// Connections pool
pub(super) struct ConnectionPool<T> {
    connector: Rc<T>,
    inner: Rc<RefCell<Inner>>,
    waiters: Rc<RefCell<Waiters>>,
}

impl<T> ConnectionPool<T>
where
    T: Service<Connect, Response = IoBoxed, Error = ConnectError> + Unpin + 'static,
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
        let waiters = Rc::new(RefCell::new(Waiters {
            waiters: HashMap::default(),
            pool: pool::new(),
        }));
        let inner = Rc::new(RefCell::new(Inner {
            conn_lifetime,
            conn_keep_alive,
            disconnect_timeout,
            limit,
            acquired: 0,
            available: HashMap::default(),
            connecting: HashSet::default(),
            waker: LocalWaker::new(),
            waiters: waiters.clone(),
        }));

        // start pool support future
        crate::rt::spawn(ConnectionPoolSupport {
            connector: connector.clone(),
            inner: inner.clone(),
            waiters: waiters.clone(),
        });

        ConnectionPool {
            connector,
            inner,
            waiters,
        }
    }
}

impl<T> Drop for ConnectionPool<T> {
    fn drop(&mut self) {
        self.inner.borrow().waker.wake();
    }
}

impl<T> Clone for ConnectionPool<T> {
    fn clone(&self) -> Self {
        ConnectionPool {
            connector: self.connector.clone(),
            inner: self.inner.clone(),
            waiters: self.waiters.clone(),
        }
    }
}

impl<T> Service<Connect> for ConnectionPool<T>
where
    T: Service<Connect, Response = IoBoxed, Error = ConnectError> + 'static,
    T::Future: Unpin,
{
    type Response = Connection;
    type Error = ConnectError;
    type Future = Pin<Box<dyn Future<Output = Result<Connection, ConnectError>>>>;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.connector.poll_ready(cx)
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        self.connector.poll_shutdown(cx, is_error)
    }

    #[inline]
    fn call(&self, req: Connect) -> Self::Future {
        trace!("Request connection to {:?}", req.uri);
        let connector = self.connector.clone();
        let inner = self.inner.clone();
        let waiters = self.waiters.clone();

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
                    trace!("Use existing {:?} connection for {:?}", io, req.uri);
                    Ok(Connection::new(
                        io,
                        created,
                        Some(Acquired::new(key, inner)),
                    ))
                }
                // open new tcp connection
                Acquire::Available => {
                    trace!("Connecting to {:?}", req.uri);
                    let (tx, rx) = waiters.borrow_mut().pool.channel();
                    OpenConnection::spawn(key, tx, inner, connector.call(req));

                    match rx.await {
                        Err(_) => Err(ConnectError::Disconnected(None)),
                        Ok(res) => res,
                    }
                }
                // pool is full, wait
                Acquire::NotAvailable => {
                    trace!(
                        "Pool is full, waiting for available connections for {:?}",
                        req.uri
                    );
                    let rx = waiters.borrow_mut().wait_for(req);
                    match rx.await {
                        Err(_) => Err(ConnectError::Disconnected(None)),
                        Ok(res) => res,
                    }
                }
            }
        })
    }
}

pub(super) struct Inner {
    conn_lifetime: Duration,
    conn_keep_alive: Duration,
    disconnect_timeout: Millis,
    limit: usize,
    acquired: usize,
    available: HashMap<Key, VecDeque<AvailableConnection>>,
    connecting: HashSet<Key>,
    waker: LocalWaker,
    waiters: Rc<RefCell<Waiters>>,
}

struct Waiters {
    waiters: HashMap<Key, VecDeque<(Connect, Waiter)>>,
    pool: pool::Pool<Result<Connection, ConnectError>>,
}

impl Waiters {
    /// connection is not available, wait
    fn wait_for(&mut self, connect: Connect) -> WaiterReceiver {
        let (tx, rx) = self.pool.channel();
        let key: Key = connect.uri.authority().unwrap().clone().into();
        self.waiters
            .entry(key)
            .or_insert_with(VecDeque::new)
            .push_back((connect, tx));
        rx
    }

    /// cleanup dropped waiters
    fn cleanup(&mut self) {
        let mut keys = Vec::new();

        // cleanup waiters
        for (key, waiters) in &mut self.waiters {
            while !waiters.is_empty() {
                let (_, tx) = waiters.front().unwrap();
                // check if waiter is still alive
                if tx.is_canceled() {
                    waiters.pop_front();
                    continue;
                };
                break;
            }

            if waiters.is_empty() {
                keys.push(key.clone());
            }
        }

        for key in keys {
            self.waiters.remove(&key);
        }
    }
}

impl Inner {
    fn acquire(&mut self, key: &Key) -> Acquire {
        // check limits
        if self.limit > 0 && self.acquired >= self.limit {
            return Acquire::NotAvailable;
        }

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
                    continue;
                }

                let io = conn.io;

                match io {
                    ConnectionType::H1(ref s) => {
                        if s.is_closed() {
                            continue;
                        }
                        let is_valid = s.with_read_buf(|buf| {
                            if buf.is_empty() || (buf.len() == 2 && &buf[..] == b"\r\n") {
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
                    ConnectionType::H2(ref s) => {
                        if s.is_closed() {
                            continue;
                        }
                        let conn = AvailableConnection {
                            io: ConnectionType::H2(s.clone()),
                            used: now,
                            created: conn.created,
                        };
                        connections.push_front(conn);
                    }
                }
                return Acquire::Acquired(io, conn.created);
            }
        }

        if self.connecting.contains(key) {
            Acquire::NotAvailable
        } else {
            Acquire::Available
        }
    }

    fn release_conn(&mut self, key: &Key, io: ConnectionType, created: Instant) {
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
        if let ConnectionType::H1(io) = io {
            spawn(async move {
                let _ = io.shutdown().await;
            });
        }
        self.check_availibility();
    }

    fn check_availibility(&mut self) {
        let mut waiters = self.waiters.borrow_mut();
        waiters.cleanup();
        if !waiters.waiters.is_empty() && self.acquired < self.limit {
            self.waker.wake();
        }
    }
}

struct ConnectionPoolSupport<T> {
    connector: T,
    inner: Rc<RefCell<Inner>>,
    waiters: Rc<RefCell<Waiters>>,
}

impl<T> Future for ConnectionPoolSupport<T>
where
    T: Service<Connect, Response = IoBoxed, Error = ConnectError> + Unpin,
    T::Future: Unpin + 'static,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // we are last copy
        if Rc::strong_count(&this.inner) == 1 {
            return Poll::Ready(());
        }

        let mut cleanup = false;
        let mut waiters = this.waiters.borrow_mut();
        this.inner.borrow_mut().waker.register(cx.waker());

        // check waiters
        for (key, waiters) in &mut waiters.waiters {
            while let Some((_, tx)) = waiters.front() {
                // is waiter still alive
                if tx.is_canceled() {
                    waiters.pop_front();
                    continue;
                };

                let result = this.inner.borrow_mut().acquire(key);
                match result {
                    Acquire::NotAvailable => break,
                    Acquire::Acquired(io, created) => {
                        cleanup = true;
                        let (_, tx) = waiters.pop_front().unwrap();
                        let _ = tx.send(Ok(Connection::new(
                            io,
                            created,
                            Some(Acquired::new(key.clone(), this.inner.clone())),
                        )));
                    }
                    Acquire::Available => {
                        let (connect, tx) = waiters.pop_front().unwrap();
                        OpenConnection::spawn(
                            key.clone(),
                            tx,
                            this.inner.clone(),
                            this.connector.call(connect),
                        );
                    }
                }
            }
        }

        if cleanup {
            waiters.cleanup()
        }

        Poll::Pending
    }
}

type H2Future = Box<
    dyn Future<
        Output = Result<(SendRequest<Bytes>, H2Connection<TokioIoBoxed, Bytes>), h2::Error>,
    >,
>;

struct OpenConnection<F> {
    key: Key,
    fut: F,
    h2: Option<Pin<H2Future>>,
    tx: Option<Waiter>,
    guard: Option<OpenGuard>,
    disconnect_timeout: Millis,
    inner: Rc<RefCell<Inner>>,
}

impl<F> OpenConnection<F>
where
    F: Future<Output = Result<IoBoxed, ConnectError>> + Unpin + 'static,
{
    fn spawn(key: Key, tx: Waiter, inner: Rc<RefCell<Inner>>, fut: F) {
        inner.borrow_mut().connecting.insert(key.clone());
        let disconnect_timeout = inner.borrow().disconnect_timeout;

        spawn(OpenConnection {
            fut,
            disconnect_timeout,
            h2: None,
            tx: Some(tx),
            key: key.clone(),
            inner: inner.clone(),
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
            return match ready!(Pin::new(h2).poll(cx)) {
                Ok((snd, connection)) => {
                    // h2 connection is ready
                    let h2 = H2Sender::new(snd);
                    let conn = Connection::new(
                        ConnectionType::H2(h2.clone()),
                        now(),
                        Some(this.guard.take().unwrap().consume()),
                    );
                    if this.tx.take().unwrap().send(Ok(conn)).is_err() {
                        // waiter is gone, return connection to pool
                        log::trace!("Waiter if gone while connecting to host");
                    }

                    let mut inner = this.inner.borrow_mut();
                    inner.connecting.remove(&this.key);
                    inner.release_conn(&this.key, ConnectionType::H2(h2.clone()), now());
                    inner.waker.wake();

                    let key = this.key.clone();
                    spawn(async move {
                        let res = connection.await;
                        h2.close();
                        log::trace!(
                            "Http/2 connection is closed for {:?} with {:?}",
                            key.authority,
                            res
                        );
                    });
                    Poll::Ready(())
                }
                Err(err) => {
                    if let Some(rx) = this.tx.take() {
                        let _ = rx.send(Err(ConnectError::H2(err)));
                    }
                    Poll::Ready(())
                }
            };
        }

        // open tcp connection
        match ready!(Pin::new(&mut this.fut).poll(cx)) {
            Err(err) => {
                trace!("Failed to open client connection {:?}", err);
                if let Some(rx) = this.tx.take() {
                    let _ = rx.send(Err(err));
                }
                Poll::Ready(())
            }
            Ok(io) => {
                io.set_disconnect_timeout(this.disconnect_timeout);

                // handle http2 proto
                if io.query::<HttpProtocol>().get() == Some(HttpProtocol::Http2) {
                    log::trace!("Connection is established, start http2 handshake");
                    // init http2 handshake
                    this.h2 =
                        Some(Box::pin(Builder::new().handshake(TokioIoBoxed::from(io))));
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
                    let mut inner = this.inner.borrow_mut();
                    inner.connecting.remove(&this.key);
                    inner.waker.wake();
                    Poll::Ready(())
                }
            }
        }
    }
}

struct OpenGuard {
    key: Key,
    inner: Option<Rc<RefCell<Inner>>>,
}

impl OpenGuard {
    fn consume(mut self) -> Acquired {
        Acquired::new(self.key.clone(), self.inner.take().unwrap())
    }
}

impl Drop for OpenGuard {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            inner.borrow_mut().check_availibility();
        }
    }
}

pub(super) struct Acquired(Key, Option<Rc<RefCell<Inner>>>);

impl Acquired {
    fn new(key: Key, inner: Rc<RefCell<Inner>>) -> Self {
        inner.borrow_mut().acquired += 1;
        Acquired(key, Some(inner))
    }

    pub(super) fn close(&mut self, conn: Connection) {
        if let Some(inner) = self.1.take() {
            let (io, _) = conn.into_inner();
            let mut inner = inner.borrow_mut();
            inner.acquired -= 1;
            inner.release_close(io);
        }
    }

    pub(super) fn release(&mut self, conn: Connection) {
        if let Some(inner) = self.1.take() {
            let (io, created) = conn.into_inner();
            let mut inner = inner.borrow_mut();
            inner.acquired -= 1;
            inner.release_conn(&self.0, io, created);
        }
    }
}

impl Drop for Acquired {
    fn drop(&mut self) {
        if let Some(inner) = self.1.take() {
            let mut inner = inner.borrow_mut();
            inner.acquired -= 1;
            inner.check_availibility();
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
                Box::pin(async move { Ok(IoBoxed::from(nio::Io::new(client))) })
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
        assert_eq!(pool.inner.borrow().acquired, 1);

        // pool is full, waiting
        let mut fut = pool.call(req.clone());
        assert!(lazy(|cx| Pin::new(&mut fut).poll(cx)).await.is_pending());
        assert_eq!(pool.waiters.borrow().waiters.len(), 1);

        // release connection and push it to next waiter
        conn.release();
        let _conn = fut.await.unwrap();
        assert_eq!(store.borrow().len(), 1);
        assert!(pool.waiters.borrow().waiters.is_empty());

        // drop waiter, no interest in connection
        let mut fut = pool.call(req.clone());
        assert!(lazy(|cx| Pin::new(&mut fut).poll(cx)).await.is_pending());
        drop(fut);
        sleep(Millis(50)).await;
        pool.inner.borrow_mut().check_availibility();
        assert!(pool.waiters.borrow().waiters.is_empty());

        assert!(lazy(|cx| pool.poll_ready(cx)).await.is_ready());
        assert!(lazy(|cx| pool.poll_shutdown(cx, false)).await.is_ready());
    }
}
