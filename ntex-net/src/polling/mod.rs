pub(crate) mod connect;
mod driver;
mod io;
mod stream;

pub use self::driver::{Driver, DriverApi, Handler};
pub use ntex_polling::{Event, PollMode};

#[cfg(not(target_pointer_width = "64"))]
compile_error!("Only 64bit platforms are supported");

/// Tcp stream wrapper for neon TcpStream
struct TcpStream(socket2::Socket, stream::StreamOps);

/// Tcp stream wrapper for neon UnixStream
struct UnixStream(socket2::Socket, stream::StreamOps);
