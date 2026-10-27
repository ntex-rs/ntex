use std::future::{pending, poll_fn};

use crate::{Handle, System, testing::TestRunner};

/// STATEMENT: Native join handles are pointer-sized and support cancellation and completion.
///
/// HOW:
/// - Cancel a pending task, await a returned value, and observe another handle finish.
/// - Check runtime notification and join-error formatting.
#[test]
fn join_handle() {
    assert_eq!(size_of::<crate::JoinHandle<usize>>(), size_of::<usize>());
    System::new("test", TestRunner).block_on(async {
        let hnd = crate::spawn(pending::<()>());
        assert!(!hnd.is_finished());
        hnd.cancel();

        assert_eq!(crate::spawn(async { 1 }).await.unwrap(), 1);

        let hnd = crate::spawn(async {});
        poll_fn(|cx| {
            if hnd.is_finished() {
                std::task::Poll::Ready(())
            } else {
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        })
        .await;

        let hnd = Handle::current().clone();
        hnd.notify().unwrap();
    });
    assert_eq!(super::JoinError.to_string(), "JoinError");
    assert_eq!(crate::rt::JoinError.to_string(), "JoinError");
}
