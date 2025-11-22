use std::{io::Result, net, net::SocketAddr};

use ntex_io::{Io, IoConfig};
use socket2::Socket;

pub(crate) mod connect;
mod driver;
mod io;

#[cfg(not(target_pointer_width = "64"))]
compile_error!("Only 64bit platforms are supported");

/// Tcp stream wrapper for neon TcpStream
struct TcpStream(socket2::Socket);

/// Tcp stream wrapper for neon UnixStream
struct UnixStream(socket2::Socket);

/// Opens a TCP connection to a remote host.
pub async fn tcp_connect(addr: SocketAddr, cfg: IoConfig) -> Result<Io> {
    let sock = crate::helpers::connect(addr).await?;
    Ok(Io::new(TcpStream(crate::helpers::prep_socket(sock)?), cfg))
}

/// Opens a unix stream connection.
pub async fn unix_connect<'a, P>(addr: P, cfg: IoConfig) -> Result<Io>
where
    P: AsRef<std::path::Path> + 'a,
{
    let sock = crate::helpers::connect_unix(addr).await?;
    Ok(Io::new(UnixStream(crate::helpers::prep_socket(sock)?), cfg))
}

/// Convert std TcpStream to TcpStream
pub fn from_tcp_stream(stream: net::TcpStream, cfg: IoConfig) -> Result<Io> {
    stream.set_nodelay(true)?;
    Ok(Io::new(
        TcpStream(crate::helpers::prep_socket(Socket::from(stream))?),
        cfg,
    ))
}

/// Convert std UnixStream to UnixStream
pub fn from_unix_stream(
    stream: std::os::unix::net::UnixStream,
    cfg: IoConfig,
) -> Result<Io> {
    Ok(Io::new(
        UnixStream(crate::helpers::prep_socket(Socket::from(stream))?),
        cfg,
    ))
}

#[doc(hidden)]
/// Get number of active Io objects
pub fn active_stream_ops() -> usize {
    self::driver::StreamOps::active_ops()
}

#[cfg(all(target_os = "linux", feature = "neon"))]
#[cfg(test)]
mod tests {
    use ntex::{io::Io, time::sleep, time::Millis, util::PoolId};
    use std::sync::{Arc, Mutex};

    use crate::connect::Connect;

    const DATA: &[u8] = b"Hello World Hello World Hello World Hello World Hello World \
                         Hello World Hello World Hello World Hello World Hello World \
                         Hello World Hello World Hello World Hello World Hello World \
                         Hello World Hello World Hello World Hello World Hello World \
                         Hello World Hello World Hello World Hello World Hello World \
                         Hello World Hello World Hello World Hello World Hello World \
                         Hello World Hello World Hello World Hello World Hello World \
                         Hello World Hello World Hello World Hello World Hello World \
                         Hello World Hello World Hello World Hello World Hello World \
                         Hello World Hello World Hello World Hello World Hello World \
                         Hello World Hello World Hello World Hello World Hello World \
                         Hello World Hello World Hello World Hello World Hello World \
                         Hello World Hello World Hello World Hello World Hello World \
                         Hello World Hello World Hello World Hello World Hello World \
                         Hello World Hello World Hello World Hello World Hello World \
                         Hello World Hello World Hello World Hello World Hello World \
                         Hello World Hello World Hello World Hello World Hello World \
                         Hello World Hello World Hello World Hello World Hello World \
                         Hello World Hello World Hello World Hello World Hello World \
                         Hello World Hello World Hello World Hello World Hello World \
                         Hello World Hello World Hello World Hello World Hello World";

    #[ntex::test]
    async fn idle_disconnect() {
        PoolId::P5.set_read_params(24, 12);
        let (tx, rx) = ::oneshot::channel();
        let tx = Arc::new(Mutex::new(Some(tx)));

        let server = ntex::server::test_server(move || {
            let tx = tx.clone();
            ntex_service::fn_service(move |io: Io<_>| {
                tx.lock().unwrap().take().unwrap().send(()).unwrap();

                async move {
                    io.write(DATA).unwrap();
                    sleep(Millis(250)).await;
                    io.close();
                    Ok::<_, ()>(())
                }
            })
        });

        let msg = Connect::new(server.addr());
        let io = crate::connect::connect(msg).await.unwrap();
        io.set_memory_pool(PoolId::P5.into());
        rx.await.unwrap();

        io.on_disconnect().await;
    }
}
