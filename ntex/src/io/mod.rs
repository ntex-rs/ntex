use std::{io, task::Context, task::Poll};

mod dispatcher;
mod filter;
mod state;
mod tasks;
mod time;
mod tokio_impl;

use crate::util::BytesMut;

pub use self::dispatcher::Dispatcher;
pub use self::state::{IoState, Read, Write};
pub use self::tasks::{ReadState, WriteState};
pub use self::time::Timer;
pub use crate::framed::DispatchItem;

pub use self::tokio_impl::TcpStream;

pub enum WriteReadiness {
    Shutdown,
    Terminate,
}

pub trait ReadFilter {
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), ()>>;

    fn read_closed(&self, err: Option<io::Error>);

    fn get_read_buf(&self) -> Option<BytesMut>;

    fn release_read_buf(&self, buf: BytesMut, new_bytes: usize);
}

pub trait WriteFilter {
    fn poll_write_ready(&self, cx: &mut Context<'_>)
        -> Poll<Result<(), WriteReadiness>>;

    fn write_closed(&self, err: Option<io::Error>);

    fn get_write_buf(&self) -> Option<BytesMut>;

    fn release_write_buf(&self, buf: BytesMut);
}

pub trait Filter: ReadFilter + WriteFilter {}

pub trait FilterFactory<F: Filter> {
    type Filter: Filter;

    fn create(inner: F) -> Self::Filter;
}

pub trait IoStream {
    fn start(self, _: ReadState, _: WriteState);
}
