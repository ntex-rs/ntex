mod connect;
mod io;
mod reactor;
mod stream;

pub use self::reactor::{Handler, Reactor, ReactorApi};
pub use ::ntex_polling::{Event, PollMode};

#[cfg(not(target_pointer_width = "64"))]
compile_error!("Only 64bit platforms are supported");

/// Slab slot of a registration and the generation of the registration in it.
///
/// The reactor runs tasks between chunks of the events of one poll. A task
/// can release a slot and a new registration can reuse it while events of the
/// old one are still queued, the generation keeps them from reaching the new
/// registration.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Key {
    idx: u32,
    generation: u16,
}

impl Key {
    const fn new(idx: u32, generation: u16) -> Self {
        Key { idx, generation }
    }

    /// Id passed to `ReactorApi`
    fn id(self) -> u64 {
        u64::from(self.idx) | (u64::from(self.generation) << 32)
    }

    /// Key of an id passed to a `Handler`
    #[allow(clippy::cast_possible_truncation)]
    const fn from_id(id: usize) -> Self {
        Key {
            idx: id as u32,
            generation: (id >> 32) as u16,
        }
    }
}

/// Tcp stream wrapper for neon `TcpStream`
///
/// The flag tells whether the socket is still attached to the reactor by the
/// connect that opened it.
struct TcpStream(socket2::Socket, stream::StreamOps, bool);

/// Tcp stream wrapper for neon `UnixStream`
///
/// The flag tells whether the socket is still attached to the reactor by the
/// connect that opened it.
struct UnixStream(socket2::Socket, stream::StreamOps, bool);
