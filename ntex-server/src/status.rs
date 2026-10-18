/// Server status.
///
/// Returned by [`Server::status`](crate::Server::status) and
/// [`Server::status_changed`](crate::Server::status_changed). The server
/// accepts new items, and network listeners accept connections, only while
/// the status is [`Ready`](ServerStatus::Ready).
#[non_exhaustive]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ServerStatus {
    /// No worker has become available yet.
    Starting,
    /// Workers are available and the server accepts new items.
    Ready,
    /// The server has been paused with [`Server::pause`](crate::Server::pause).
    ///
    /// The server stays paused until [`Server::resume`](crate::Server::resume)
    /// is called, even if workers restart or become available.
    Paused,
    /// No worker is available, e.g. all workers are restarting or not ready.
    ///
    /// The server becomes ready again once a worker is available.
    Unavailable,
    /// The server is stopping its workers.
    Stopping,
    /// The server has stopped. This status is final.
    Stopped,
}

impl ServerStatus {
    pub(crate) const fn from_u8(val: u8) -> Self {
        match val {
            0 => ServerStatus::Starting,
            1 => ServerStatus::Ready,
            2 => ServerStatus::Paused,
            3 => ServerStatus::Unavailable,
            4 => ServerStatus::Stopping,
            _ => ServerStatus::Stopped,
        }
    }

    pub(crate) const fn as_u8(self) -> u8 {
        match self {
            ServerStatus::Starting => 0,
            ServerStatus::Ready => 1,
            ServerStatus::Paused => 2,
            ServerStatus::Unavailable => 3,
            ServerStatus::Stopping => 4,
            ServerStatus::Stopped => 5,
        }
    }
}
