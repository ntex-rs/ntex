use std::{future::Future, io, pin::Pin, task::Context, task::Poll};

use crate::driver::{Key, OpCode, PushEntry};
use crate::rt::Runtime;

#[derive(Debug)]
pub(crate) struct OpFuture<T: OpCode> {
    key: Option<Key<T>>,
}

impl<T: OpCode> OpFuture<T> {
    pub(crate) fn new(key: Key<T>) -> Self {
        Self { key: Some(key) }
    }
}

impl<T: OpCode> Future for OpFuture<T> {
    type Output = (io::Result<usize>, T);

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let res = Runtime::with_current(|r| r.poll_task(cx, self.key.take().unwrap()));
        match res {
            PushEntry::Pending(key) => {
                self.key = Some(key);
                Poll::Pending
            }
            PushEntry::Ready(res) => Poll::Ready(res),
        }
    }
}

impl<T: OpCode> Drop for OpFuture<T> {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            Runtime::with_current(|r| r.cancel_op(key));
        }
    }
}
