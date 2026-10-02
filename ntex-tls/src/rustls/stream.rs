use std::{any, cell::RefCell, cmp, io, mem, task::Poll};

use ntex_bytes::{BufMut, BytePages, BytesMut, buf::UninitSlice};
use ntex_io::{FilterBuf, types};
use tls_rustls::client::{ClientConnectionData, UnbufferedClientConnection};
use tls_rustls::server::{Acceptor, ServerConnectionData, UnbufferedServerConnection};
use tls_rustls::unbuffered::{
    ConnectionState, EncodeError, EncryptError, ReadEarlyData, UnbufferedStatus, WriteTraffic,
};
use tls_rustls::{CommonState, Error};

use super::{PeerCert, PeerCertChain};

/// Largest plaintext of a tls record
const MAX_RECORD: usize = 16 * 1024;
/// Upper bound of the record overhead: header, tls 1.2 explicit nonce and aead tag
pub(super) const OVERHEAD: usize = 5 + 8 + 16;
/// Smallest record worth encrypting into the rest of the current page
const MIN_RECORD: usize = 1024;

thread_local! {
    static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static OUTPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Unbuffered rustls connection
pub(crate) trait Session {
    type Data;

    fn common(&self) -> &CommonState;

    fn process<'c, 'i>(
        &'c mut self,
        incoming: &'i mut [u8],
    ) -> UnbufferedStatus<'c, 'i, Self::Data>;

    /// Drops early data, returns the number of bytes to discard.
    fn drop_early_data(state: &mut ReadEarlyData<'_, '_, Self::Data>) -> Result<usize, Error>;
}

impl Session for UnbufferedClientConnection {
    type Data = ClientConnectionData;

    fn common(&self) -> &CommonState {
        self
    }

    fn process<'c, 'i>(
        &'c mut self,
        incoming: &'i mut [u8],
    ) -> UnbufferedStatus<'c, 'i, Self::Data> {
        self.process_tls_records(incoming)
    }

    fn drop_early_data(_: &mut ReadEarlyData<'_, '_, Self::Data>) -> Result<usize, Error> {
        Ok(0)
    }
}

/// Server connection
pub(crate) struct ServerSession {
    conn: UnbufferedServerConnection,
    /// Parses the client hello for the server name, unbuffered connections
    /// do not expose it. Holds the number of input bytes passed to it.
    acceptor: Option<(Acceptor, usize)>,
    server_name: Option<String>,
}

impl ServerSession {
    pub(crate) fn new(conn: UnbufferedServerConnection) -> Self {
        Self {
            conn,
            acceptor: Some((Acceptor::default(), 0)),
            server_name: None,
        }
    }

    pub(crate) fn server_name(&self) -> Option<&str> {
        self.server_name.as_deref()
    }

    fn parse_client_hello(&mut self, incoming: &[u8]) {
        let Some((acceptor, fed)) = &mut self.acceptor else {
            return;
        };
        if !self.conn.is_handshaking() {
            self.acceptor = None;
            return;
        }

        let mut rd = incoming.get(*fed..).unwrap_or_default();
        while !rd.is_empty() {
            match acceptor.read_tls(&mut rd) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        *fed = incoming.len() - rd.len();

        match acceptor.accept() {
            Ok(None) => {}
            Ok(Some(accepted)) => {
                self.server_name = accepted.client_hello().server_name().map(String::from);
                self.acceptor = None;
            }
            Err(_) => self.acceptor = None,
        }
    }
}

impl Session for ServerSession {
    type Data = ServerConnectionData;

    fn common(&self) -> &CommonState {
        &self.conn
    }

    fn process<'c, 'i>(
        &'c mut self,
        incoming: &'i mut [u8],
    ) -> UnbufferedStatus<'c, 'i, Self::Data> {
        if self.acceptor.is_some() {
            self.parse_client_hello(incoming);
        }
        let status = self.conn.process_tls_records(incoming);
        if let Some((_, fed)) = &mut self.acceptor {
            *fed = fed.saturating_sub(status.discard);
        }
        status
    }

    fn drop_early_data(state: &mut ReadEarlyData<'_, '_, Self::Data>) -> Result<usize, Error> {
        let mut discard = 0;
        while let Some(record) = state.next_record() {
            discard += record?.discard;
        }
        Ok(discard)
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Close {
    Open,
    /// Shutdown is requested
    Requested,
    /// `close_notify` is queued
    Sent,
}

pub(crate) struct Stream<S> {
    pub(crate) session: S,
    close: Close,
    /// Peer sent `close_notify`
    peer_closed: bool,
    /// Session failed, it must not process input again
    failed: bool,
}

impl<S: Session> Stream<S> {
    pub(crate) fn new(session: S) -> Self {
        Self {
            session,
            close: Close::Open,
            peer_closed: false,
            failed: false,
        }
    }

    pub(crate) fn is_handshaking(&self) -> bool {
        self.session.common().is_handshaking()
    }

    pub(crate) fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        const H2: &[u8] = b"h2";

        let session = self.session.common();
        if id == any::TypeId::of::<types::HttpProtocol>() {
            let h2 = session
                .alpn_protocol()
                .is_some_and(|protos| protos.windows(2).any(|w| w == H2));

            let proto = if h2 {
                types::HttpProtocol::Http2
            } else {
                types::HttpProtocol::Http1
            };
            Some(Box::new(proto))
        } else if id == any::TypeId::of::<PeerCert<'_>>() {
            let cert = session.peer_certificates()?.first()?;
            Some(Box::new(PeerCert(cert.to_owned())))
        } else if id == any::TypeId::of::<PeerCertChain<'_>>() {
            let chain = session.peer_certificates()?;
            Some(Box::new(PeerCertChain(chain.to_vec())))
        } else {
            None
        }
    }

    pub(crate) fn process(&mut self, buf: &FilterBuf<'_>) -> io::Result<()> {
        buf.with_read_buffers(|r_src, r_dst| {
            buf.with_write_buffers(|w_src, w_dst| self.drive(buf, r_src, r_dst, w_src, w_dst))
        })
    }

    pub(crate) fn shutdown(&mut self, buf: &FilterBuf<'_>) -> io::Result<Poll<()>> {
        if self.failed {
            return Ok(Poll::Ready(()));
        }

        // pending application data is encrypted before close_notify
        if self.close == Close::Open {
            self.close = Close::Requested;
        }
        self.process(buf)?;

        // wait for the peer's close_notify, unless the peer already closed
        // the connection and it is never going to arrive. close_notify cannot
        // be sent before the handshake completes.
        if self.close != Close::Sent || self.peer_closed || buf.io().is_read_eof() {
            Ok(Poll::Ready(()))
        } else {
            Ok(Poll::Pending)
        }
    }

    /// Processes the incoming tls records and encrypts the pending application
    /// data.
    ///
    /// The session keeps offsets into `src` between calls while a handshake
    /// message is incomplete, so every call gets the same input buffer.
    fn drive(
        &mut self,
        buf: &FilterBuf<'_>,
        src: &mut Option<BytesMut>,
        dst: &mut BytesMut,
        w_src: &mut BytePages,
        w_dst: &mut BytePages,
    ) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "TLS session has failed",
            ));
        }

        // plaintext is never larger than the input records
        if let Some(src) = src {
            dst.reserve(src.len());
        }

        // input records are processed
        let mut processed = false;
        loop {
            let UnbufferedStatus { mut discard, state } =
                self.session.process(src.as_deref_mut().unwrap_or_default());
            processed |= discard != 0;

            let result = match state {
                Ok(ConnectionState::ReadTraffic(mut state)) => {
                    let mut result = Ok(false);
                    while let Some(record) = state.next_record() {
                        match record {
                            Ok(record) => {
                                discard += record.discard;
                                dst.extend_from_slice(record.payload);
                            }
                            Err(err) => {
                                result = Err(err);
                                break;
                            }
                        }
                    }
                    result
                }
                Ok(ConnectionState::ReadEarlyData(mut state)) => S::drop_early_data(&mut state)
                    .map(|n| {
                        discard += n;
                        false
                    }),
                Ok(ConnectionState::EncodeTlsData(mut state)) => {
                    write_tls(w_dst, |out| encoded(state.encode(out)))?;
                    Ok(false)
                }
                Ok(ConnectionState::TransmitTlsData(state)) => {
                    // the records are in the output buffer already
                    state.done();
                    Ok(false)
                }
                Ok(ConnectionState::PeerClosed) => {
                    // peer sent close_notify, start graceful shutdown
                    self.peer_closed = true;
                    buf.io().close();
                    Ok(false)
                }
                Ok(ConnectionState::WriteTraffic(mut state)) => {
                    let mut done = true;
                    if self.close != Close::Sent {
                        encrypt_pages(&mut state, w_src, w_dst)?;
                        if self.close == Close::Requested {
                            write_tls(w_dst, |out| encrypted(state.queue_close_notify(out)))?;
                            self.close = Close::Sent;
                        } else if mem::take(&mut processed) {
                            // rustls holds the response to a peer's KeyUpdate
                            // until the next record is encrypted, encrypting
                            // into an empty buffer queues it for output
                            let _ = state.encrypt(&[0], &mut []);
                            done = !self.session.common().wants_write();
                        }
                    }
                    Ok(done)
                }
                // handshake waits for input, or the connection is closed
                Ok(_) => Ok(true),
                Err(err) => Err(err),
            };

            if discard != 0
                && let Some(src) = src
            {
                src.advance_to(discard);
            }

            match result {
                Ok(false) => {}
                Ok(true) => break,
                Err(err) => {
                    self.failed = true;
                    self.write_alert(src, w_dst);
                    return Err(io::Error::new(io::ErrorKind::InvalidData, err));
                }
            }
        }

        // input after close_notify is ignored
        if self.peer_closed
            && let Some(src) = src
        {
            src.clear();
        }
        Ok(())
    }

    /// Writes the alert queued by a failed session, if any.
    ///
    /// The session fails again if it deframes the input, it is only called
    /// while it holds tls data, which is returned before any input is read.
    fn write_alert(&mut self, src: &mut Option<BytesMut>, w_dst: &mut BytePages) {
        while self.session.common().wants_write() {
            let UnbufferedStatus { discard, state } =
                self.session.process(src.as_deref_mut().unwrap_or_default());

            let written = match state {
                Ok(ConnectionState::EncodeTlsData(mut state)) => {
                    write_tls(w_dst, |out| encoded(state.encode(out))).is_ok()
                }
                _ => false,
            };

            if discard != 0
                && let Some(src) = src
            {
                src.advance_to(discard);
            }
            if !written {
                break;
            }
        }
    }
}
fn encoded(res: Result<usize, EncodeError>) -> io::Result<Result<usize, usize>> {
    match res {
        Ok(n) => Ok(Ok(n)),
        Err(EncodeError::InsufficientSize(err)) => Ok(Err(err.required_size)),
        Err(err) => Err(io::Error::other(err)),
    }
}

fn encrypted(res: Result<usize, EncryptError>) -> io::Result<Result<usize, usize>> {
    match res {
        Ok(n) => Ok(Ok(n)),
        Err(EncryptError::InsufficientSize(err)) => Ok(Err(err.required_size)),
        Err(err) => Err(io::Error::other(err)),
    }
}

fn as_slice(chunk: &mut UninitSlice) -> &mut [u8] {
    // tls output is written, never read
    unsafe { &mut *(&raw mut *chunk as *mut [u8]) }
}

/// Writes tls data into the free space of the current output page, or copies
/// it into the pages if it does not fit.
///
/// `f` returns the number of written bytes, or the required size if the
/// buffer is too small.
fn write_tls<F>(dst: &mut BytePages, f: F) -> io::Result<()>
where
    F: FnMut(&mut [u8]) -> io::Result<Result<usize, usize>>,
{
    write_tls_with(dst, true, 0, f)
}

fn write_tls_with<F>(dst: &mut BytePages, in_place: bool, size: usize, mut f: F) -> io::Result<()>
where
    F: FnMut(&mut [u8]) -> io::Result<Result<usize, usize>>,
{
    let mut size = size;
    if in_place {
        match f(as_slice(dst.chunk_mut()))? {
            Ok(n) => {
                unsafe { dst.advance_mut(n) };
                return Ok(());
            }
            Err(required) => size = required,
        }
    }

    // the data is copied into the pages, it fills the rest of the current
    // page and continues on new pages
    OUTPUT.with_borrow_mut(|out| {
        out.clear();
        out.reserve(size);
        loop {
            // tls output is written, never read
            let chunk = unsafe { &mut *(&raw mut *out.spare_capacity_mut() as *mut [u8]) };
            match f(chunk)? {
                Ok(n) => {
                    unsafe { out.set_len(n) };
                    dst.put_slice(out);
                    if out.capacity() > MAX_RECORD * 2 {
                        *out = Vec::new();
                    }
                    return Ok(());
                }
                Err(required) => out.reserve(required),
            }
        }
    })
}

/// Encrypts the write buffer into tls records in the output buffer.
///
/// A record is encrypted in place into the free space of the current output
/// page. A large record is shrunk to fit the free space, unless the free
/// space or the rest of the record is small, then the record is encrypted
/// into a scratch buffer and copied into the pages.
pub(super) fn encrypt_pages<D>(
    state: &mut WriteTraffic<'_, D>,
    src: &mut BytePages,
    dst: &mut BytePages,
) -> io::Result<()> {
    while let Some(mut page) = src.take() {
        let len = cmp::min(page.len() + src.len(), MAX_RECORD);
        let room = dst.chunk_mut().len().saturating_sub(OVERHEAD);
        let in_place = len <= room || (room >= MIN_RECORD && len - room >= MIN_RECORD);
        let size = if in_place { cmp::min(len, room) } else { len };

        if page.len() < cmp::min(size, MAX_RECORD / 2) && !src.is_empty() {
            // every write produces its own record, so a small page, e.g.
            // response headers in front of a body, is joined with the
            // following pages
            SCRATCH.with_borrow_mut(|buf| {
                buf.clear();
                buf.extend_from_slice(&page);
                while buf.len() < size {
                    let Some(mut next) = src.take() else {
                        break;
                    };
                    let n = cmp::min(next.len(), size - buf.len());
                    buf.extend_from_slice(&next[..n]);
                    next.advance_to(n);
                    src.prepend(next);
                }
                encrypt(state, buf, dst, in_place)
            })?;
        } else {
            let n = cmp::min(page.len(), size);
            encrypt(state, &page[..n], dst, in_place)?;
            page.advance_to(n);
            src.prepend(page);
        }
    }
    Ok(())
}

fn encrypt<D>(
    state: &mut WriteTraffic<'_, D>,
    data: &[u8],
    dst: &mut BytePages,
    in_place: bool,
) -> io::Result<()> {
    write_tls_with(dst, in_place, data.len() + OVERHEAD, |out| {
        encrypted(state.encrypt(data, out))
    })
}
