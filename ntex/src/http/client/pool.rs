use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use std::{cell::RefCell, collections::VecDeque, future::Future, pin::Pin, rc::Rc};

use ntex_h2::{self as h2};

use crate::http::uri::{Authority, Scheme, Uri};
use crate::io::{types::HttpProtocol, IoBoxed};
use crate::service::{Container, Ctx, Service, ServiceCall};
use crate::time::{now, Millis};
use crate::util::{ready, BoxFuture, ByteString, HashMap, HashSet};
use crate::{channel::pool, rt::spawn, task::LocalWaker};

use super::connection::{Connection, ConnectionType};
use super::h2proto::{H2Client, H2PublishService};
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
    connector: Container<T>,
    inner: Rc<RefCell<Inner>>,
    waiters: Rc<RefCell<Waiters>>,
}

impl<T> ConnectionPool<T>
where
    T: Service<Connect, Response = IoBoxed, Error = ConnectError> + 'static,
{
    pub(super) fn new(
        connector: T,
        conn_lifetime: Duration,
        conn_keep_alive: Duration,
        disconnect_timeout: Millis,
        limit: usize,
        h2config: h2::Config,
    ) -> Self {
        let connector = Container::new(connector);
        let waiters = Rc::new(RefCell::new(Waiters {
            waiters: HashMap::default(),
            pool: pool::new(),
        }));
        let inner = Rc::new(RefCell::new(Inner {
            conn_lifetime,
            conn_keep_alive,
            disconnect_timeout,
            limit,
            h2config,
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
{
    type Response = Connection;
    type Error = ConnectError;
    type Future<'f> = BoxFuture<'f, Result<Connection, ConnectError>>;

    crate::forward_poll_ready!(connector);
    crate::forward_poll_shutdown!(connector);

    fn call<'a>(&'a self, req: Connect, _: Ctx<'a, Self>) -> Self::Future<'_> {
        trace!("Get connection for {:?}", req.uri);
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
                    let uri = req.uri.clone();
                    let (tx, rx) = waiters.borrow_mut().pool.channel();
                    OpenConnection::spawn(key, tx, uri, inner, self.connector.clone(), req);

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
    h2config: h2::Config,
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
                let (req, tx) = waiters.front().unwrap();
                // check if waiter is still alive
                if tx.is_canceled() {
                    trace!("Waiter for {:?} is gone, remove waiter", req.uri);
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

    fn check_availibility(&mut self) {
        let mut waiters = self.waiters.borrow_mut();
        waiters.cleanup();
        if !waiters.waiters.is_empty() && self.acquired < self.limit {
            self.waker.wake();
        }
    }
}

struct ConnectionPoolSupport<T> {
    connector: Container<T>,
    inner: Rc<RefCell<Inner>>,
    waiters: Rc<RefCell<Waiters>>,
}

impl<T> Future for ConnectionPoolSupport<T>
where
    T: Service<Connect, Response = IoBoxed, Error = ConnectError> + 'static,
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
            while let Some((req, tx)) = waiters.front() {
                // is waiter still alive
                if tx.is_canceled() {
                    trace!("Waiter for {:?} is gone, cleanup", req.uri);
                    cleanup = true;
                    waiters.pop_front();
                    continue;
                };

                let result = this.inner.borrow_mut().acquire(key);
                match result {
                    Acquire::NotAvailable => break,
                    Acquire::Acquired(io, created) => {
                        trace!(
                            "Use existing {:?} connection for {:?}, wake up waiter",
                            io,
                            req.uri
                        );
                        cleanup = true;
                        let (_, tx) = waiters.pop_front().unwrap();
                        let _ = tx.send(Ok(Connection::new(
                            io,
                            created,
                            Some(Acquired::new(key.clone(), this.inner.clone())),
                        )));
                    }
                    Acquire::Available => {
                        trace!("Connecting to {:?} and wake up waiter", req.uri);
                        cleanup = true;
                        let (connect, tx) = waiters.pop_front().unwrap();
                        let uri = connect.uri.clone();
                        OpenConnection::spawn(
                            key.clone(),
                            tx,
                            uri,
                            this.inner.clone(),
                            this.connector.clone(),
                            connect,
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

pin_project_lite::pin_project! {
    struct OpenConnection<'f, T: Service<Connect>>
    where T: 'f
    {
        key: Key,
        #[pin]
        fut: ServiceCall<'f, T, Connect>,
        uri: Uri,
        tx: Option<Waiter>,
        guard: Option<OpenGuard>,
        disconnect_timeout: Millis,
        inner: Rc<RefCell<Inner>>,
    }
}

impl<'f, T> OpenConnection<'f, T>
where
    T: Service<Connect, Response = IoBoxed, Error = ConnectError> + 'static,
{
    fn spawn(
        key: Key,
        tx: Waiter,
        uri: Uri,
        inner: Rc<RefCell<Inner>>,
        connector: Container<T>,
        msg: Connect,
    ) {
        let disconnect_timeout = inner.borrow().disconnect_timeout;

        #[allow(clippy::redundant_async_block)]
        spawn(async move {
            OpenConnection::<T> {
                fut: connector.call(msg),
                tx: Some(tx),
                key: key.clone(),
                inner: inner.clone(),
                guard: Some(OpenGuard::new(key, inner)),
                uri,
                disconnect_timeout,
            }
            .await
        });
    }
}

impl<'f, T> Future for OpenConnection<'f, T>
where
    T: Service<Connect, Response = IoBoxed, Error = ConnectError>,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        // open tcp connection
        match ready!(this.fut.poll(cx)) {
            Err(err) => {
                trace!(
                    "Failed to open client connection for {:?} with error {:?}",
                    &this.key.authority,
                    err
                );
                let _ = this.guard.take();
                if let Some(rx) = this.tx.take() {
                    let _ = rx.send(Err(err));
                }
                Poll::Ready(())
            }
            Ok(io) => {
                io.set_disconnect_timeout(*this.disconnect_timeout);

                // handle http2 proto
                if io.query::<HttpProtocol>().get() == Some(HttpProtocol::Http2) {
                    // init http2 handshake
                    log::trace!(
                        "Connection for {:?} is established, start http2 handshake",
                        &this.key.authority
                    );
                    let auth = if let Some(auth) = this.uri.authority() {
                        format!("{}", auth).into()
                    } else {
                        ByteString::new()
                    };

                    let connection = h2::client::ClientConnection::with_params(
                        io,
                        this.inner.borrow().h2config.clone(),
                        this.uri.scheme() == Some(&Scheme::HTTPS),
                        auth,
                    );
                    let client = H2Client::new(connection.client());
                    let key = this.key.clone();
                    let publish = H2PublishService::new(client.clone());
                    crate::rt::spawn(async move {
                        let res = connection.start(publish).await;
                        log::trace!(
                            "Http/2 connection is closed for {:?} with {:?}",
                            key.authority,
                            res
                        );
                    });

                    let guard = this.guard.take().unwrap().consume();
                    let conn = Connection::new(
                        ConnectionType::H2(client.clone()),
                        now(),
                        Some(guard.clone()),
                    );
                    if this.tx.take().unwrap().send(Ok(conn)).is_err() {
                        // waiter is gone, return connection to pool
                        log::trace!(
                            "Waiter for {:?} is gone while connecting to host",
                            &this.key.authority
                        );
                    }

                    // put h2 connection to list of available connections
                    Connection::new(ConnectionType::H2(client), now(), Some(guard))
                        .release(false);

                    Poll::Ready(())
                } else {
                    log::trace!(
                        "Connection for {:?} is established, init http1 connection",
                        &this.key.authority
                    );
                    let conn = Connection::new(
                        ConnectionType::H1(io),
                        now(),
                        Some(this.guard.take().unwrap().consume()),
                    );
                    if let Err(Ok(conn)) = this.tx.take().unwrap().send(Ok(conn)) {
                        // waiter is gone, return connection to pool
                        conn.release(false)
                    }
                    this.inner.borrow_mut().check_availibility();
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
    fn new(key: Key, inner: Rc<RefCell<Inner>>) -> Self {
        inner.borrow_mut().connecting.insert(key.clone());
        OpenGuard {
            key,
            inner: Some(inner),
        }
    }

    fn consume(mut self) -> Acquired {
        let inner = self.inner.take().unwrap();
        inner.borrow_mut().connecting.remove(&self.key);
        Acquired::new(self.key.clone(), inner)
    }
}

impl Drop for OpenGuard {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            let mut pool = inner.borrow_mut();
            pool.connecting.remove(&self.key);
            pool.check_availibility();
        }
    }
}

pub(super) struct Acquired(Key, Option<Rc<RefCell<Inner>>>);

impl Acquired {
    fn new(key: Key, inner: Rc<RefCell<Inner>>) -> Self {
        inner.borrow_mut().acquired += 1;
        Acquired(key, Some(inner))
    }

    fn clone(&self) -> Self {
        Acquired::new(self.0.clone(), self.1.as_ref().unwrap().clone())
    }

    pub(super) fn release(&mut self, conn: Connection, close: bool) {
        if let Some(inner) = self.1.take() {
            let (io, created, _) = conn.into_inner();
            let mut inner = inner.borrow_mut();
            inner.acquired -= 1;
            if close {
                log::trace!(
                    "Releasing and closing connection for {:?}",
                    self.0.authority
                );
                match io {
                    ConnectionType::H1(io) => {
                        spawn(async move {
                            let _ = io.shutdown().await;
                        });
                    }
                    ConnectionType::H2(io) => io.close(),
                }
            } else {
                log::trace!("Releasing connection for {:?}", self.0.authority);
                inner
                    .available
                    .entry(self.0.clone())
                    .or_insert_with(VecDeque::new)
                    .push_back(AvailableConnection {
                        io,
                        created,
                        used: now(),
                    });
            }
            inner.check_availibility();
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
    use std::{cell::RefCell, rc::Rc};

    use super::*;
    use crate::{
        http::Uri, io as nio, service::fn_service, testing::Io, time::sleep, util::lazy,
    };

    #[crate::rt_test]
    async fn test_basics() {
        let store = Rc::new(RefCell::new(Vec::new()));
        let store2 = store.clone();

        let pool = Container::new(
            ConnectionPool::new(
                fn_service(move |req| {
                    let (client, server) = Io::create();
                    store2.borrow_mut().push((req, server));
                    Box::pin(async move { Ok(IoBoxed::from(nio::Io::new(client))) })
                }),
                Duration::from_secs(10),
                Duration::from_secs(10),
                Millis::ZERO,
                1,
                h2::Config::client(),
            )
            .clone(),
        );

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
        assert!(pool.inner.borrow().connecting.is_empty());

        // pool is full, waiting
        let mut fut = pool.call(req.clone());
        assert!(lazy(|cx| Pin::new(&mut fut).poll(cx)).await.is_pending());
        assert_eq!(pool.waiters.borrow().waiters.len(), 1);

        // release connection and push it to next waiter
        conn.release(false);
        assert_eq!(pool.inner.borrow().acquired, 0);
        let _conn = fut.await.unwrap();
        assert_eq!(store.borrow().len(), 1);
        assert!(pool.waiters.borrow().waiters.is_empty());
        drop(_conn);

        // close connnection
        let conn = pool.call(req.clone()).await.unwrap();
        assert_eq!(store.borrow().len(), 2);
        assert_eq!(pool.inner.borrow().acquired, 1);
        assert!(pool.inner.borrow().connecting.is_empty());
        let mut fut = pool.call(req.clone());
        assert!(lazy(|cx| Pin::new(&mut fut).poll(cx)).await.is_pending());
        assert_eq!(pool.waiters.borrow().waiters.len(), 1);

        // release and close
        conn.release(true);
        assert_eq!(pool.inner.borrow().acquired, 0);
        assert!(pool.inner.borrow().connecting.is_empty());

        let conn = fut.await.unwrap();
        assert_eq!(store.borrow().len(), 3);
        assert!(pool.waiters.borrow().waiters.is_empty());
        assert!(pool.inner.borrow().connecting.is_empty());
        assert_eq!(pool.inner.borrow().acquired, 1);

        // drop waiter, no interest in connection
        let mut fut = pool.call(req.clone());
        assert!(lazy(|cx| Pin::new(&mut fut).poll(cx)).await.is_pending());
        drop(fut);
        sleep(Millis(50)).await;
        pool.inner.borrow_mut().check_availibility();
        assert!(pool.waiters.borrow().waiters.is_empty());

        // different uri
        let req = Connect {
            uri: Uri::try_from("http://localhost2/test").unwrap(),
            addr: None,
        };
        let mut fut = pool.call(req.clone());
        assert!(lazy(|cx| Pin::new(&mut fut).poll(cx)).await.is_pending());
        assert_eq!(pool.waiters.borrow().waiters.len(), 1);
        conn.release(false);
        assert_eq!(pool.inner.borrow().acquired, 0);
        assert_eq!(pool.inner.borrow().available.len(), 1);

        let conn = fut.await.unwrap();
        assert_eq!(store.borrow().len(), 4);
        assert!(pool.waiters.borrow().waiters.is_empty());
        assert!(pool.inner.borrow().connecting.is_empty());
        assert_eq!(pool.inner.borrow().acquired, 1);
        conn.release(false);
        assert_eq!(pool.inner.borrow().acquired, 0);
        assert_eq!(pool.inner.borrow().available.len(), 2);

        assert!(lazy(|cx| pool.poll_ready(cx)).await.is_ready());
        assert!(lazy(|cx| pool.poll_shutdown(cx)).await.is_ready());
    }
}
