use std::time::Instant;
use std::{cell::Cell, cell::RefCell, collections::VecDeque, fmt, future, rc::Rc};

use ntex_h2::{self as h2};

use crate::error::Error;
use crate::http::uri::{Authority, Scheme, Uri};
use crate::io::{IoBoxed, types::HttpProtocol};
use crate::service::pipeline::{PipelineBinding, PipelineCall};
use crate::service::{Ctx, Service, cfg::Cfg, cfg::SharedCfg};
use crate::util::{ByteString, Either, HashMap, HashSet, select};
use crate::{channel::inplace, channel::oneshot, channel::pool, rt::spawn, time::now};

use super::connection::{Connection, ConnectionType};
use super::{ClientConfig, Connect, ConnectorPipeline, error::ConnectError, h2proto::H2Client};

#[derive(Hash, Eq, PartialEq, Clone, Debug)]
pub(super) struct Key {
    authority: Authority,
}

impl From<Authority> for Key {
    fn from(authority: Authority) -> Key {
        Key { authority }
    }
}

type Waiter = pool::Sender<Result<Connection, Error<ConnectError>>>;
type WaiterReceiver = pool::Receiver<Result<Connection, Error<ConnectError>>>;

enum Acquire {
    Acquired(ConnectionType, Instant),
    Available,
    NotAvailable,
}

#[derive(Debug)]
struct AvailableConnection {
    io: IoBoxed,
    used: Instant,
    created: Instant,
}

/// Connections pool
pub(super) struct ConnectionPool(Rc<ConnectionPoolInner>);

struct ConnectionPoolInner {
    cfg: SharedCfg,
    svc: ConnectorPipeline,
    inner: Rc<RefCell<Inner>>,
    waiters: Rc<RefCell<Waiters>>,
    stop: Rc<Cell<Option<oneshot::Sender<()>>>>,
}

#[derive(Debug)]
pub(super) struct Inner {
    cfg: Cfg<ClientConfig>,
    stopped: bool,
    acquired: usize,
    available: HashMap<Key, VecDeque<AvailableConnection>>,
    h2: HashMap<Key, Vec<H2Client>>,
    connecting: HashSet<Key>,
    waker: inplace::Inplace<()>,
    waiters: Rc<RefCell<Waiters>>,
}

impl ConnectionPool {
    pub(super) fn new(svc: ConnectorPipeline, cfg: Cfg<ClientConfig>) -> Self {
        let shared = cfg.shared();
        let waiters = Rc::new(RefCell::new(Waiters {
            waiters: HashMap::default(),
            pool: pool::new(),
        }));
        let inner = Rc::new(RefCell::new(Inner {
            cfg,
            stopped: false,
            acquired: 0,
            available: HashMap::default(),
            h2: HashMap::default(),
            connecting: HashSet::default(),
            waker: inplace::channel(),
            waiters: waiters.clone(),
        }));

        // start connection pool
        let (stop, stop_rx) = oneshot::channel();
        crate::rt::spawn(run_connection_pool(
            shared.clone(),
            svc.bind_state(shared.clone()),
            inner.clone(),
            waiters.clone(),
            stop_rx,
        ));

        ConnectionPool(Rc::new(ConnectionPoolInner {
            svc,
            inner,
            waiters,
            cfg: shared,
            stop: Rc::new(Cell::new(Some(stop))),
        }))
    }
}

impl Drop for ConnectionPool {
    fn drop(&mut self) {
        if Rc::strong_count(&self.0) == 1 {
            self.0.stop.take();
            self.0.inner.borrow_mut().stop();
        }
    }
}

impl Clone for ConnectionPool {
    fn clone(&self) -> Self {
        ConnectionPool(self.0.clone())
    }
}

impl fmt::Debug for ConnectionPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionPool")
            .field("svc", &self.0.svc)
            .field("inner", &self.0.inner)
            .field("waiters", &self.0.waiters)
            .finish()
    }
}

impl Service<SharedCfg, Connect> for ConnectionPool {
    type Res = Connection;
    type Error = Error<ConnectError>;

    #[inline]
    async fn ready(&self, ctx: Ctx<'_, Self, SharedCfg>) -> Result<(), Self::Error> {
        self.0.svc.ready(ctx.st()).await
    }

    #[inline]
    async fn shutdown(&self, ctx: Ctx<'_, Self, SharedCfg>) {
        self.0.stop.take();
        self.0.inner.borrow_mut().stop();
        self.0.svc.shutdown(ctx.st()).await;
    }

    async fn call(
        &self,
        req: Connect,
        ctx: Ctx<'_, Self, SharedCfg>,
    ) -> Result<Self::Res, Self::Error> {
        log::trace!("{}: Get connection for {:?}", ctx.st().tag(), req.uri);

        let inner = self.0.inner.clone();
        let waiters = self.0.waiters.clone();

        let key = if let Some(authority) = req.uri.authority() {
            authority.clone().into()
        } else {
            return Err(ConnectError::Unresolved.into());
        };

        if inner.borrow().stopped {
            return Err(ConnectError::Disconnected(None).into());
        }

        // acquire connection
        let result = inner.borrow_mut().acquire(&key);
        match result {
            // use existing connection
            Acquire::Acquired(io, created) => {
                log::trace!(
                    "{}: Use existing {:?} connection for {:?}",
                    ctx.st().tag(),
                    io,
                    req.uri
                );
                // http/2 requests are not counted by the connection limit
                let pool = matches!(io, ConnectionType::H1(_)).then(|| Acquired::new(key, inner));
                Ok(Connection::new(io, created, pool))
            }
            // open new tcp connection
            Acquire::Available => {
                log::trace!("{}: Connecting to {:?}", ctx.st().tag(), req.uri);
                let uri = req.uri.clone();
                let (tx, rx) = waiters.borrow_mut().pool.channel();
                open_connection(
                    self.0.cfg.clone(),
                    req,
                    key,
                    tx,
                    uri,
                    inner,
                    self.0.svc.bind_state(self.0.cfg.clone()),
                );

                match rx.await {
                    Err(_) => Err(ConnectError::Disconnected(None).into()),
                    Ok(result) => result,
                }
            }
            // pool is full, wait
            Acquire::NotAvailable => {
                log::trace!(
                    "{}: Pool is full, waiting for available connections for {:?}",
                    ctx.st().tag(),
                    req.uri
                );
                let rx = waiters.borrow_mut().wait_for(req);
                match rx.await {
                    Err(_) => Err(ConnectError::Disconnected(None).into()),
                    Ok(result) => result,
                }
            }
        }
    }
}

#[derive(Debug)]
struct Waiters {
    waiters: HashMap<Key, VecDeque<(Connect, Waiter)>>,
    pool: pool::Pool<Result<Connection, Error<ConnectError>>>,
}

impl Waiters {
    /// connection is not available, wait
    fn wait_for(&mut self, connect: Connect) -> WaiterReceiver {
        let (tx, rx) = self.pool.channel();
        let key: Key = connect.uri.authority().unwrap().clone().into();
        self.waiters
            .entry(key)
            .or_default()
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
                    log::trace!("Waiter for {:?} is gone, remove waiter", req.uri);
                    waiters.pop_front();
                    continue;
                }
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
    /// Stops the pool, fails pending waiters and closes pooled connections
    fn stop(&mut self) {
        self.stopped = true;
        let _ = self.waker.send(());

        // waiters receive `Disconnected` error
        let waiters = std::mem::take(&mut self.waiters.borrow_mut().waiters);
        drop(waiters);

        for conn in self.available.drain().flat_map(|(_, conns)| conns) {
            let io = conn.io;
            spawn(async move {
                let _ = io.shutdown().await;
            });
        }
        // disconnects after in-flight streams are completed
        for conn in self.h2.drain().flat_map(|(_, conns)| conns) {
            conn.close();
        }
    }

    fn acquire(&mut self, key: &Key) -> Acquire {
        let now = now();

        // shared http/2 connections, cleanup stale connections at the same time
        let mut h2_saturated = 0;
        if let Some(connections) = self.h2.get_mut(key) {
            let cfg = &self.cfg;
            connections.retain(|conn| {
                if conn.is_closed() || conn.is_disconnecting() {
                    return false;
                }
                let expired = (!cfg.h2_lifetime.is_zero()
                    && (now - conn.created()) > cfg.h2_lifetime)
                    || (!cfg.h2_keep_alive.is_zero()
                        && conn.is_idle()
                        && (now - conn.used()) > cfg.h2_keep_alive);
                if expired {
                    // disconnects after in-flight streams are completed
                    conn.close();
                }
                !expired
            });

            // use least loaded connection
            let conn = connections
                .iter()
                .filter(|conn| conn.has_capacity(cfg.h2_max_streams))
                .min_by_key(|conn| conn.streams());
            if let Some(conn) = conn {
                return Acquire::Acquired(ConnectionType::H2(conn.begin()), conn.created());
            }
            h2_saturated = connections.len();
            if connections.is_empty() {
                self.h2.remove(key);
            }
        }

        // all http/2 connections are busy
        if h2_saturated > 0
            && ((self.cfg.h2_limit > 0 && h2_saturated >= self.cfg.h2_limit)
                || self.connecting.contains(key))
        {
            return Acquire::NotAvailable;
        }

        // check limits
        if self.cfg.h1_connection_limit() > 0 && self.acquired >= self.cfg.h1_connection_limit() {
            return Acquire::NotAvailable;
        }

        // check if open connection is available
        // cleanup stale connections at the same time
        if let Some(ref mut connections) = self.available.get_mut(key) {
            while let Some(conn) = connections.pop_back() {
                // check if it still usable
                if (now - conn.used) > self.cfg.h1_keep_alive
                    || (now - conn.created) > self.cfg.h1_lifetime
                {
                    let io = conn.io;
                    spawn(async move {
                        let _ = io.shutdown().await;
                    });
                    continue;
                }

                let io = conn.io;
                if !io.is_active() || io.is_read_eof() {
                    continue;
                }
                let is_valid = io.with_read_dst(|buf| {
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
                return Acquire::Acquired(ConnectionType::H1(io), conn.created);
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
        // http/2 capacity does not depend on the connection limit
        if !waiters.waiters.is_empty() {
            let _ = self.waker.send(());
        }
    }
}

async fn run_connection_pool(
    cfg: SharedCfg,
    svc: PipelineBinding<Connect, IoBoxed, Error<ConnectError>>,
    inner: Rc<RefCell<Inner>>,
    waiters: Rc<RefCell<Waiters>>,
    mut stop: oneshot::Receiver<()>,
) {
    log::trace!("{}: Starting connection pool support task", cfg.tag());

    loop {
        {
            let mut cleanup = false;
            let mut waiters = waiters.borrow_mut();

            // check waiters
            for (key, waiters) in &mut waiters.waiters {
                while let Some((req, tx)) = waiters.front() {
                    // is waiter still alive
                    if tx.is_canceled() {
                        log::trace!("{}: Waiter for {:?} is gone, cleanup", cfg.tag(), req.uri);
                        cleanup = true;
                        waiters.pop_front();
                        continue;
                    }

                    let result = inner.borrow_mut().acquire(key);
                    match result {
                        Acquire::NotAvailable => break,
                        Acquire::Acquired(io, created) => {
                            log::trace!(
                                "{}: Use existing {:?} connection for {:?}, wake up waiter",
                                cfg.tag(),
                                io,
                                req.uri
                            );
                            cleanup = true;
                            let (_, tx) = waiters.pop_front().unwrap();
                            let pool = matches!(io, ConnectionType::H1(_))
                                .then(|| Acquired::new(key.clone(), inner.clone()));
                            let _ = tx.send(Ok(Connection::new(io, created, pool)));
                        }
                        Acquire::Available => {
                            log::trace!(
                                "{}: Connecting to {:?} and wake up waiter",
                                cfg.tag(),
                                req.uri
                            );
                            cleanup = true;
                            let (connect, tx) = waiters.pop_front().unwrap();
                            let uri = connect.uri.clone();
                            open_connection(
                                cfg.clone(),
                                connect,
                                key.clone(),
                                tx,
                                uri,
                                inner.clone(),
                                svc.clone(),
                            );
                        }
                    }
                }
            }

            if cleanup {
                waiters.cleanup();
            }
        }

        let result = select(
            &mut stop,
            future::poll_fn(|cx| inner.borrow().waker.poll_recv(cx)),
        )
        .await;

        if matches!(result, Either::Left(_)) || inner.borrow().stopped {
            log::trace!("{}: Stopping connection pool support task", cfg.tag());
            break;
        }
    }
}

pin_project_lite::pin_project! {
    struct OpenConnection {
        key: Key,
        #[pin]
        fut: PipelineCall<Connect, IoBoxed, Error<ConnectError>>,
        uri: Uri,
        tx: Option<Waiter>,
        guard: Option<OpenGuard>,
        inner: Rc<RefCell<Inner>>,
    }
}

fn open_connection(
    cfg: SharedCfg,
    connect: Connect,
    key: Key,
    tx: Waiter,
    uri: Uri,
    inner: Rc<RefCell<Inner>>,
    pl: PipelineBinding<Connect, IoBoxed, Error<ConnectError>>,
) {
    let guard = OpenGuard::new(key.clone(), inner.clone());

    spawn(async move {
        // open tcp connection
        match pl.call(connect).await {
            Err(err) => {
                log::trace!(
                    "Failed to open client connection for {:?} with error {:?}",
                    key.authority,
                    err
                );
                let _ = tx.send(Err(err));
            }
            Ok(io) => {
                if inner.borrow().stopped {
                    return;
                }

                // handle http2 proto
                if io.query::<HttpProtocol>().get() == Some(HttpProtocol::Http2) {
                    // init http2 handshake
                    log::trace!(
                        "{}: Connection for {:?} is established, start http2 handshake",
                        io.tag(),
                        key.authority
                    );
                    let auth = if let Some(auth) = uri.authority() {
                        format!("{auth}").into()
                    } else {
                        ByteString::new()
                    };

                    let client = h2::client::SimpleClient::new(
                        io,
                        uri.scheme().cloned().unwrap_or(Scheme::HTTPS),
                        auth,
                    );
                    let conn = add_h2_client(&inner, &key, client).begin();
                    // wake up waiters, connection can be shared
                    drop(guard);

                    let conn = Connection::new(ConnectionType::H2(conn), now(), None);
                    if tx.send(Ok(conn)).is_err() {
                        log::trace!(
                            "{}: Waiter for {:?} is gone while connecting to host",
                            cfg.tag(),
                            key.authority
                        );
                    }
                } else {
                    log::trace!(
                        "{}: Connection for {:?} is established, init http1 connection",
                        io.tag(),
                        key.authority
                    );
                    let conn =
                        Connection::new(ConnectionType::H1(io), now(), Some(guard.consume()));
                    if let Err(Ok(conn)) = tx.send(Ok(conn)) {
                        // waiter is gone, return connection to pool
                        conn.release(false);
                    }
                    inner.borrow_mut().check_availibility();
                }
            }
        }
    });
}

/// Adds shared http/2 connection to the pool
fn add_h2_client(
    inner: &Rc<RefCell<Inner>>,
    key: &Key,
    client: h2::client::SimpleClient,
) -> H2Client {
    let weak = Rc::downgrade(inner);
    let client = H2Client::new(client, move || {
        // request is completed, stream is available
        if let Some(inner) = weak.upgrade()
            && let Ok(mut inner) = inner.try_borrow_mut()
        {
            inner.check_availibility();
        }
    });
    inner
        .borrow_mut()
        .h2
        .entry(key.clone())
        .or_default()
        .push(client.clone());
    client
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

    pub(super) fn release(&mut self, conn: Connection, close: bool) {
        if let Some(inner) = self.1.take() {
            let (io, created, _) = conn.into_inner();
            let mut inner = inner.borrow_mut();
            inner.acquired -= 1;
            // http/2 connections are shared and stay in the pool
            let ConnectionType::H1(io) = io else {
                inner.check_availibility();
                return;
            };
            if close || inner.stopped || !io.is_active() || io.is_read_eof() {
                log::trace!(
                    "{:?}: Releasing and closing connection for {:?}",
                    io.tag(),
                    self.0.authority
                );
                spawn(async move {
                    let _ = io.shutdown().await;
                });
            } else {
                log::trace!(
                    "{:?}: Releasing connection for {:?}",
                    io.tag(),
                    self.0.authority
                );
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
    use std::{future::Future, pin::Pin};

    use super::*;
    use crate::service::{Pipeline, boxed, fn_service};
    use crate::time::{Millis, Seconds, sleep};
    use crate::{io as nio, testing::IoTest, util::lazy};

    #[crate::rt_test]
    async fn test_unlimited_concurrent_connect() {
        let store = Rc::new(RefCell::new(Vec::new()));
        let store2 = store.clone();

        let cfg = SharedCfg::new("C")
            .add(ClientConfig::new().set_h1_connection_limit(0))
            .build();
        let pool = ConnectionPool::new(
            ConnectorPipeline::new(boxed::service(fn_service(move |_| {
                let (client, server) = IoTest::create();
                store2.borrow_mut().push(server);
                Box::pin(async move {
                    sleep(Millis(10)).await;
                    Ok(IoBoxed::from(nio::Io::new(client, SharedCfg::default())))
                })
            }))),
            cfg.get(),
        );
        let pipe = Pipeline::new(cfg, pool.clone());
        let req = Connect {
            uri: Uri::try_from("http://localhost/test").unwrap(),
            addr: None,
        };

        // second request waits for the pending connect to the same host
        let (c1, c2) = crate::time::timeout(
            Millis(1000),
            crate::util::join(pipe.call(req.clone()), pipe.call(req.clone())),
        )
        .await
        .unwrap();
        assert!(c1.is_ok());
        assert!(c2.is_ok());
        assert_eq!(store.borrow().len(), 2);
        assert_eq!(pool.0.inner.borrow().acquired, 2);
    }

    fn h2_pool(
        cfg: ClientConfig,
    ) -> (
        Pipeline<Connect, Connection, Error<ConnectError>>,
        ConnectionPool,
    ) {
        let cfg = SharedCfg::new("C").add(cfg).build();
        let pool = ConnectionPool::new(
            ConnectorPipeline::new(boxed::service(fn_service(|_| {
                Box::pin(async { Err(Error::from(ConnectError::Unresolved)) })
            }))),
            cfg.get(),
        );
        (Pipeline::new(cfg, pool.clone()), pool)
    }

    fn h2_conn(pool: &ConnectionPool) -> (H2Client, IoTest) {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(64 * 1024);
        let io = nio::Io::new(client, SharedCfg::default());
        let client = h2::client::SimpleClient::new(
            IoBoxed::from(io),
            Scheme::HTTP,
            ByteString::from_static("localhost"),
        );
        let key = Key::from(Authority::from_static("localhost"));
        (add_h2_client(&pool.0.inner, &key, client), server)
    }

    fn h2_key() -> Key {
        Key::from(Authority::from_static("localhost"))
    }

    fn secs_ago(secs: u64) -> Instant {
        now()
            .checked_sub(std::time::Duration::from_secs(secs))
            .unwrap()
    }

    fn acquire_h2(pool: &ConnectionPool) -> Option<H2Client> {
        match pool.0.inner.borrow_mut().acquire(&h2_key()) {
            Acquire::Acquired(ConnectionType::H2(conn), _) => Some(conn),
            _ => None,
        }
    }

    async fn wait_closed(h2: &H2Client) {
        // graceful disconnect, peer does not respond
        for _ in 0..60 {
            if h2.is_closed() {
                break;
            }
            sleep(Millis(50)).await;
        }
    }

    #[crate::rt_test]
    async fn test_expired_h2_is_closed() {
        let (_, pool) = h2_pool(ClientConfig::new().set_h2_keepalive(Seconds(1)));
        let (h2, server) = h2_conn(&pool);
        h2.set_times(secs_ago(2), secs_ago(2));

        assert!(matches!(
            pool.0.inner.borrow_mut().acquire(&h2_key()),
            Acquire::Available
        ));
        assert!(pool.0.inner.borrow().h2.is_empty());
        wait_closed(&h2).await;
        assert!(h2.is_closed());
        assert!(server.is_closed());
    }

    #[crate::rt_test]
    async fn test_expired_h2_lifetime() {
        let (_, pool) = h2_pool(ClientConfig::new().set_h2_lifetime(Seconds(1)));
        let (h2, server) = h2_conn(&pool);
        h2.set_times(secs_ago(2), now());

        assert!(acquire_h2(&pool).is_none());
        wait_closed(&h2).await;
        assert!(server.is_closed());
    }

    #[crate::rt_test]
    async fn test_h2_lifecycle_settings() {
        // http/1 settings do not apply to http/2 connections
        let (_, pool) = h2_pool(
            ClientConfig::new()
                .set_h1_keepalive(Seconds(1))
                .set_h1_lifetime(Seconds(1))
                .set_h2_keepalive(Seconds(5)),
        );
        let (h2, _server) = h2_conn(&pool);
        h2.set_times(secs_ago(3), secs_ago(3));
        let req = acquire_h2(&pool).unwrap();
        assert_eq!(h2.streams(), 1);

        // idle period is measured from completion of the last request
        h2.set_times(secs_ago(10), secs_ago(10));
        assert!(acquire_h2(&pool).is_some());
        assert_eq!(h2.streams(), 1);
        drop(req);
        assert_eq!(h2.streams(), 0);
        assert!(acquire_h2(&pool).is_some());
        assert!(!h2.is_disconnecting());

        // idle connection is expired
        h2.set_times(secs_ago(10), secs_ago(10));
        assert!(acquire_h2(&pool).is_none());
        assert!(pool.0.inner.borrow().h2.is_empty());
    }

    #[crate::rt_test]
    async fn test_h2_streams_limit() {
        let (pipe, pool) = h2_pool(
            ClientConfig::new()
                .set_h1_connection_limit(1)
                .set_h2_connection_limit(1)
                .set_h2_max_streams(2),
        );
        let (h2, _server) = h2_conn(&pool);

        let req = Connect {
            uri: Uri::try_from("http://localhost/test").unwrap(),
            addr: None,
        };

        // http/2 requests do not count against the connection limit
        let req1 = pipe.call(req.clone()).await.unwrap();
        let req2 = acquire_h2(&pool).unwrap();
        assert_eq!(h2.streams(), 2);
        assert_eq!(pool.0.inner.borrow().acquired, 0);

        // connection is saturated, h2 connection limit is reached
        assert!(matches!(
            pool.0.inner.borrow_mut().acquire(&h2_key()),
            Acquire::NotAvailable
        ));

        // waiter is woken up when a request completes,
        // even if the connection limit is reached by http/1 connections
        pool.0.inner.borrow_mut().acquired = 1;
        let mut fut = std::pin::pin!(pipe.call(req));
        assert!(lazy(|cx| fut.as_mut().poll(cx)).await.is_pending());
        sleep(Millis(50)).await;
        assert!(lazy(|cx| fut.as_mut().poll(cx)).await.is_pending());
        drop(req1);
        let conn = crate::time::timeout(Millis(1000), fut)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(conn.protocol(), HttpProtocol::Http2);
        assert_eq!(h2.streams(), 2);
        assert_eq!(pool.0.inner.borrow().acquired, 1);
        pool.0.inner.borrow_mut().acquired = 0;
        drop(conn);
        drop(req2);
        assert_eq!(h2.streams(), 0);
    }

    #[crate::rt_test]
    async fn test_h2_new_connection_when_saturated() {
        let (_, pool) = h2_pool(
            ClientConfig::new()
                .set_h2_connection_limit(2)
                .set_h2_max_streams(2),
        );
        let (h1, _s1) = h2_conn(&pool);
        let _req1 = acquire_h2(&pool).unwrap();
        let req2 = acquire_h2(&pool).unwrap();
        assert!(matches!(
            pool.0.inner.borrow_mut().acquire(&h2_key()),
            Acquire::Available
        ));

        // least loaded connection is used
        let (h2, _s2) = h2_conn(&pool);
        drop(req2);
        let _req3 = acquire_h2(&pool).unwrap();
        assert_eq!(h1.streams(), 1);
        assert_eq!(h2.streams(), 1);
        let _req4 = acquire_h2(&pool).unwrap();
        let _req5 = acquire_h2(&pool).unwrap();
        assert_eq!(h1.streams(), 2);
        assert_eq!(h2.streams(), 2);
        assert!(matches!(
            pool.0.inner.borrow_mut().acquire(&h2_key()),
            Acquire::NotAvailable
        ));
    }

    #[crate::rt_test]
    async fn test_shutdown_closes_pool() {
        let store = Rc::new(RefCell::new(Vec::new()));
        let store2 = store.clone();

        let cfg = SharedCfg::new("C")
            .add(ClientConfig::new().set_h1_connection_limit(2))
            .build();
        let pool = ConnectionPool::new(
            ConnectorPipeline::new(boxed::service(fn_service(move |_| {
                let (client, server) = IoTest::create();
                store2.borrow_mut().push(server);
                Box::pin(
                    async move { Ok(IoBoxed::from(nio::Io::new(client, SharedCfg::default()))) },
                )
            }))),
            cfg.get(),
        );
        let pipe = Pipeline::new(cfg, pool.clone());
        let req = |host: &str| Connect {
            uri: Uri::try_from(format!("http://{host}/test")).unwrap(),
            addr: None,
        };

        // idle http/1 and http/2 connections
        pipe.call(req("host1")).await.unwrap().release(false);
        let (h2, h2srv) = h2_conn(&pool);
        drop(h2);

        // pool is full, waiter
        let _c2 = pipe.call(req("host2")).await.unwrap();
        let _c3 = pipe.call(req("host3")).await.unwrap();
        let mut fut = std::pin::pin!(pipe.call(req("host4")));
        assert!(lazy(|cx| fut.as_mut().poll(cx)).await.is_pending());

        pipe.shutdown().await;

        // waiter is failed
        let res = crate::time::timeout(Millis(500), fut).await.unwrap();
        assert!(res.is_err());

        // pooled connections are closed
        assert!(pool.0.inner.borrow().available.is_empty());
        assert!(pool.0.inner.borrow().h2.is_empty());
        let h1srv = store.borrow()[0].clone();
        for _ in 0..60 {
            if h1srv.is_closed() && h2srv.is_closed() {
                break;
            }
            sleep(Millis(50)).await;
        }
        assert!(h1srv.is_closed());
        assert!(h2srv.is_closed());

        // new requests are rejected
        let res = crate::time::timeout(Millis(500), pipe.call(req("host1"))).await;
        assert!(res.unwrap().is_err());
        assert_eq!(store.borrow().len(), 3);
    }

    #[crate::rt_test]
    async fn test_release_after_stop() {
        let store = Rc::new(RefCell::new(Vec::new()));
        let store2 = store.clone();

        let cfg = SharedCfg::new("C").add(ClientConfig::new()).build();
        let pool = ConnectionPool::new(
            ConnectorPipeline::new(boxed::service(fn_service(move |_| {
                let (client, server) = IoTest::create();
                store2.borrow_mut().push(server);
                Box::pin(
                    async move { Ok(IoBoxed::from(nio::Io::new(client, SharedCfg::default()))) },
                )
            }))),
            cfg.get(),
        );
        let pipe = Pipeline::new(cfg, pool.clone());
        let req = Connect {
            uri: Uri::try_from("http://localhost/test").unwrap(),
            addr: None,
        };

        let conn = pipe.call(req).await.unwrap();
        pipe.shutdown().await;
        assert!(pool.0.inner.borrow().stopped);

        conn.release(false);
        assert_eq!(pool.0.inner.borrow().acquired, 0);
        assert!(pool.0.inner.borrow().available.is_empty());
    }

    #[crate::rt_test]
    async fn test_basics() {
        let store = Rc::new(RefCell::new(Vec::new()));
        let store2 = store.clone();

        let cfg = SharedCfg::new("C")
            .add(
                ClientConfig::new()
                    .set_h1_keepalive(Seconds(10))
                    .set_h1_lifetime(Seconds(10))
                    .set_h1_connection_limit(1),
            )
            .build();

        let pool = ConnectionPool::new(
            ConnectorPipeline::new(boxed::service(fn_service(move |req| {
                let (client, server) = IoTest::create();
                store2.borrow_mut().push((req, server));
                Box::pin(
                    async move { Ok(IoBoxed::from(nio::Io::new(client, SharedCfg::default()))) },
                )
            }))),
            cfg.get(),
        );
        let pipe = Pipeline::new(cfg, pool.clone());

        // uri must contain authority
        let req = Connect {
            uri: Uri::try_from("/test").unwrap(),
            addr: None,
        };
        let _err = Error::from(ConnectError::Unresolved);
        assert!(matches!(pipe.call(req).await, Err(_err)));

        // connect one
        let req = Connect {
            uri: Uri::try_from("http://localhost/test").unwrap(),
            addr: None,
        };
        let conn = pipe.call(req.clone()).await.unwrap();
        assert_eq!(store.borrow().len(), 1);
        assert!(format!("{conn:?}").contains("Connection(h1)"));
        assert_eq!(conn.protocol(), HttpProtocol::Http1);
        assert_eq!(pool.0.inner.borrow().acquired, 1);
        assert!(pool.0.inner.borrow().connecting.is_empty());

        // pool is full, waiting
        let mut fut = std::pin::pin!(pipe.call(req.clone()));
        assert!(lazy(|cx| fut.as_mut().poll(cx)).await.is_pending());
        assert_eq!(pool.0.waiters.borrow().waiters.len(), 1);

        // release connection and push it to next waiter
        conn.release(false);
        assert_eq!(pool.0.inner.borrow().acquired, 0);
        let conn = fut.await.unwrap();
        assert_eq!(store.borrow().len(), 1);
        assert!(pool.0.waiters.borrow().waiters.is_empty());
        drop(conn);

        // close connnection
        let conn = pipe.call(req.clone()).await.unwrap();
        assert_eq!(store.borrow().len(), 2);
        assert_eq!(pool.0.inner.borrow().acquired, 1);
        assert!(pool.0.inner.borrow().connecting.is_empty());
        let mut fut = std::pin::pin!(pipe.call(req.clone()));
        assert!(lazy(|cx| fut.as_mut().poll(cx)).await.is_pending());
        assert_eq!(pool.0.waiters.borrow().waiters.len(), 1);

        // release and close
        conn.release(true);
        assert_eq!(pool.0.inner.borrow().acquired, 0);
        assert!(pool.0.inner.borrow().connecting.is_empty());

        let conn = fut.await.unwrap();
        assert_eq!(store.borrow().len(), 3);
        assert!(pool.0.waiters.borrow().waiters.is_empty());
        assert!(pool.0.inner.borrow().connecting.is_empty());
        assert_eq!(pool.0.inner.borrow().acquired, 1);

        // drop waiter, no interest in connection
        let mut fut = Box::pin(pipe.call(req.clone()));
        assert!(lazy(|cx| Pin::new(&mut fut).poll(cx)).await.is_pending());
        drop(fut);
        sleep(Millis(50)).await;
        pool.0.inner.borrow_mut().check_availibility();
        assert!(pool.0.waiters.borrow().waiters.is_empty());

        // different uri
        let req = Connect {
            uri: Uri::try_from("http://localhost2/test").unwrap(),
            addr: None,
        };
        let mut fut = std::pin::pin!(pipe.call(req.clone()));
        assert!(lazy(|cx| fut.as_mut().poll(cx)).await.is_pending());
        assert_eq!(pool.0.waiters.borrow().waiters.len(), 1);
        conn.release(false);
        assert_eq!(pool.0.inner.borrow().acquired, 0);
        assert_eq!(pool.0.inner.borrow().available.len(), 1);

        let conn = fut.await.unwrap();
        assert_eq!(store.borrow().len(), 4);
        assert!(pool.0.waiters.borrow().waiters.is_empty());
        assert!(pool.0.inner.borrow().connecting.is_empty());
        assert_eq!(pool.0.inner.borrow().acquired, 1);
        conn.release(false);
        assert_eq!(pool.0.inner.borrow().acquired, 0);
        assert_eq!(pool.0.inner.borrow().available.len(), 2);

        assert!(lazy(|cx| pipe.poll_ready(cx)).await.is_ready());
        assert!(lazy(|cx| pipe.poll_shutdown(cx)).await.is_ready());
    }

    #[crate::rt_test]
    async fn clean_eof_connections_are_not_reused() {
        let store = Rc::new(RefCell::new(Vec::new()));
        let store2 = store.clone();

        let cfg = SharedCfg::new("C")
            .add(
                ClientConfig::new()
                    .set_h1_keepalive(Seconds(10))
                    .set_h1_lifetime(Seconds(10))
                    .set_h1_connection_limit(1),
            )
            .build();

        let pool = ConnectionPool::new(
            ConnectorPipeline::new(boxed::service(fn_service(move |req| {
                let (client, server) = IoTest::create();
                store2.borrow_mut().push((req, server));
                Box::pin(
                    async move { Ok(IoBoxed::from(nio::Io::new(client, SharedCfg::default()))) },
                )
            }))),
            cfg.get(),
        );
        let pipe = Pipeline::new(cfg, pool.clone());
        let req = Connect {
            uri: Uri::try_from("http://localhost/test").unwrap(),
            addr: None,
        };

        // EOF observed before release: do not add the connection to the pool.
        let conn = pipe.call(req.clone()).await.unwrap();
        let peer = store.borrow()[0].1.clone();
        peer.close().await;
        conn.release(false);
        assert!(pool.0.inner.borrow().available.is_empty());

        // EOF observed after release: reject the stale pooled connection.
        let conn = pipe.call(req.clone()).await.unwrap();
        assert_eq!(store.borrow().len(), 2);
        let peer = store.borrow()[1].1.clone();
        conn.release(false);
        assert_eq!(pool.0.inner.borrow().available.len(), 1);
        peer.close().await;

        let conn = pipe.call(req).await.unwrap();
        assert_eq!(store.borrow().len(), 3);
        conn.release(true);
        assert!(lazy(|cx| pipe.poll_shutdown(cx)).await.is_ready());
    }
}
