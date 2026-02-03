use socket2::Socket;

pub(crate) mod connect;
mod driver;
mod io;
mod stream;

pub use self::driver::{Driver, DriverApi, Handler};

/// Tcp stream wrapper for neon `TcpStream`
struct TcpStream(Socket, stream::StreamOps);

/// Tcp stream wrapper for neon `UnixStream`
struct UnixStream(Socket, stream::StreamOps);
