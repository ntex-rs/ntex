use socket2::Socket;

mod connect;
mod io;
mod reactor;
mod stream;

pub use self::reactor::{Handler, Reactor, ReactorApi};

/// Tcp stream wrapper for neon `TcpStream`
struct TcpStream(Socket, stream::StreamOps);

/// Tcp stream wrapper for neon `UnixStream`
struct UnixStream(Socket, stream::StreamOps);
