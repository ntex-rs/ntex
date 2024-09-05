use std::{any, io};

use compio::buf::{BufResult, IoBuf, IoBufMut, SetBufInit};
use compio::io::{AsyncRead, AsyncWrite};
use compio::net::TcpStream;
use ntex_bytes::{Buf, BufMut, BytesVec};
use ntex_io::{
    types, Handle, IoStream, ReadContext, ReadStatus, WriteContext, WriteStatus,
};
use ntex_util::{future::select, future::Either, time::sleep};

impl IoStream for crate::TcpStream {
    fn start(self, read: ReadContext, write: WriteContext) -> Option<Box<dyn Handle>> {
        let mut io = self.0.clone();
        compio::runtime::spawn(async move {
            run(&mut io, &read, write).await;

            match io.close().await {
                Ok(_) => log::debug!("{} Stream is closed", read.tag()),
                Err(e) => log::error!("{} Stream is closed, {:?}", read.tag(), e),
            }
        })
        .detach();

        Some(Box::new(HandleWrapper(self.0)))
    }
}

#[cfg(unix)]
impl IoStream for crate::UnixStream {
    fn start(self, read: ReadContext, write: WriteContext) -> Option<Box<dyn Handle>> {
        let mut io = self.0;
        compio::runtime::spawn(async move {
            run(&mut io, &read, write).await;

            match io.close().await {
                Ok(_) => log::debug!("{} Unix stream is closed", read.tag()),
                Err(e) => log::error!("{} Unix stream is closed, {:?}", read.tag(), e),
            }
        })
        .detach();

        None
    }
}

struct HandleWrapper(TcpStream);

impl Handle for HandleWrapper {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        if id == any::TypeId::of::<types::PeerAddr>() {
            if let Ok(addr) = self.0.peer_addr() {
                return Some(Box::new(types::PeerAddr(addr)));
            }
        }
        None
    }
}

struct CompioBuf(BytesVec);

unsafe impl IoBuf for CompioBuf {
    #[inline]
    fn as_buf_ptr(&self) -> *const u8 {
        self.0.chunk().as_ptr()
    }

    #[inline]
    fn buf_len(&self) -> usize {
        self.0.len()
    }

    #[inline]
    fn buf_capacity(&self) -> usize {
        self.0.remaining_mut()
    }
}

unsafe impl IoBufMut for CompioBuf {
    fn as_buf_mut_ptr(&mut self) -> *mut u8 {
        self.0.chunk_mut().as_mut_ptr()
    }
}

impl SetBufInit for CompioBuf {
    unsafe fn set_buf_init(&mut self, len: usize) {
        self.0.set_len(len + self.0.len());
    }
}

async fn run<T: AsyncRead + AsyncWrite + Clone + 'static>(
    io: &mut T,
    read: &ReadContext,
    write: WriteContext,
) {
    let mut wr_io = io.clone();
    let wr_task = compio::runtime::spawn(async move {
        write_task(&mut wr_io, &write).await;
        log::debug!("{} Write task is stopped", write.tag());
    });

    read_task(io, read).await;
    log::debug!("{} Read task is stopped", read.tag());

    if !wr_task.is_finished() {
        let _ = wr_task.await;
    }
}

/// Read io task
async fn read_task<T: AsyncRead>(io: &mut T, state: &ReadContext) {
    loop {
        match state.ready().await {
            ReadStatus::Ready => {
                let result = state
                    .with_buf_async(|buf| async {
                        let BufResult(result, buf) =
                            match select(io.read(CompioBuf(buf)), state.wait_for_close())
                                .await
                            {
                                Either::Left(res) => res,
                                Either::Right(_) => return (Default::default(), Ok(1)),
                            };

                        match result {
                            Ok(n) => {
                                if n == 0 {
                                    log::trace!(
                                        "{}: Tcp stream is disconnected",
                                        state.tag()
                                    );
                                }
                                (buf.0, Ok(n))
                            }
                            Err(err) => {
                                log::trace!(
                                    "{}: Read task failed on io {:?}",
                                    state.tag(),
                                    err
                                );
                                (buf.0, Err(err))
                            }
                        }
                    })
                    .await;

                if result.is_ready() {
                    break;
                }
            }
            ReadStatus::Terminate => {
                log::trace!("{}: Read task is instructed to shutdown", state.tag());
                break;
            }
        }
    }
}

/// Write io task
async fn write_task<T: AsyncWrite>(mut io: T, state: &WriteContext) {
    let mut delay = None;

    loop {
        let result = if let Some(ref mut sleep) = delay {
            let result = match select(sleep, state.ready()).await {
                Either::Left(_) => {
                    state.close(Some(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "Operation timedout",
                    )));
                    return;
                }
                Either::Right(res) => res,
            };
            delay = None;
            result
        } else {
            state.ready().await
        };

        match result {
            WriteStatus::Ready => {
                // write io stream
                match write(&mut io, state).await {
                    Ok(()) => continue,
                    Err(e) => {
                        state.close(Some(e));
                    }
                }
            }
            WriteStatus::Timeout(time) => {
                log::trace!("{}: Initiate timeout delay for {:?}", state.tag(), time);
                delay = Some(sleep(time));
                continue;
            }
            WriteStatus::Shutdown(time) => {
                log::trace!("{}: Write task is instructed to shutdown", state.tag());

                let fut = async {
                    write(&mut io, state).await?;
                    io.flush().await?;
                    io.shutdown().await?;
                    Ok(())
                };
                match select(sleep(time), fut).await {
                    Either::Left(_) => state.close(None),
                    Either::Right(res) => state.close(res.err()),
                }
            }
            WriteStatus::Terminate => {
                log::trace!("{}: Write task is instructed to terminate", state.tag());
                state.close(io.shutdown().await.err());
            }
        }
        break;
    }
}

// write to io stream
async fn write<T: AsyncWrite>(io: &mut T, state: &WriteContext) -> io::Result<()> {
    state
        .with_buf_async(|buf| async {
            let mut buf = CompioBuf(buf);
            loop {
                let BufResult(result, buf1) = io.write(buf).await;
                buf = buf1;

                return match result {
                    Ok(0) => Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "failed to write frame to transport",
                    )),
                    Ok(size) => {
                        if buf.0.len() == size {
                            // return io.flush().await;
                            state.memory_pool().release_write_buf(buf.0);
                            Ok(())
                        } else {
                            buf.0.advance(size);
                            continue;
                        }
                    }
                    Err(e) => Err(e),
                };
            }
        })
        .await
}
