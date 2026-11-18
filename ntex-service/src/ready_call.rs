//! Call future that waits for readiness, then calls a service.
//!
//! All `unsafe` code of [`ReadyThenCall`] is in this module.
use std::{future::Future, mem, pin::Pin, ptr, task::Context, task::Poll, task::ready};

/// Waits for readiness, then calls the service.
///
/// Unlike an `async` block, the call state does not keep storage for `env` and
/// the request after they are moved into the service call. Both steps share
/// `env`, so the closures capture nothing and `env` is stored once. `ready` runs
/// on the first poll and returns `None` if no readiness wait is needed.
///
/// The readiness wait is boxed so that its storage, which grows with each
/// nested readiness level, is not inlined into every call future. It is only
/// allocated when the scope is not ready.
pub(crate) struct ReadyThenCall<Env, Req, R, RF, C, CF> {
    state: State<Env, Req, R, RF, C, CF>,
}

/// Pinning: only `Call::fut` is structurally pinned. It is never moved after it
/// is written, and is dropped in place. The other fields are never pinned and
/// are moved out freely. There is no `Drop` impl, and the auto `Unpin` impl
/// requires `CF: Unpin`.
enum State<Env, Req, R, RF, C, CF> {
    Init {
        env: Env,
        req: Req,
        ready: R,
        call: C,
    },
    Ready {
        fut: Pin<Box<RF>>,
        env: Env,
        req: Req,
        call: C,
    },
    Call {
        fut: CF,
    },
    /// Readiness failed, or `ready` or `call` panicked.
    Done,
}

impl<Env, Req, R, RF, C, CF> ReadyThenCall<Env, Req, R, RF, C, CF> {
    pub(crate) fn new(env: Env, req: Req, ready: R, call: C) -> Self {
        ReadyThenCall {
            state: State::Init {
                env,
                req,
                ready,
                call,
            },
        }
    }
}

impl<Env, Req, R, RF, C, CF, T, E> Future for ReadyThenCall<Env, Req, R, RF, C, CF>
where
    R: FnOnce(&Env) -> Option<RF>,
    RF: Future<Output = Result<(), E>>,
    C: FnOnce(Env, Req) -> CF,
    CF: Future<Output = Result<T, E>>,
{
    type Output = Result<T, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: only `Call::fut` is pinned, and it is never moved, see `State`
        let state = unsafe { &mut self.get_unchecked_mut().state };
        loop {
            let (env, req, call) = match state {
                State::Call { fut } => {
                    // SAFETY: `fut` stays in place until it is dropped, see `State`
                    return unsafe { Pin::new_unchecked(fut) }.poll(cx);
                }
                State::Init {
                    env,
                    req,
                    ready,
                    call,
                } => {
                    // SAFETY: the fields are not pinned. Each is read once, and
                    // `Done` overwrites them without dropping, so the locals are
                    // their only owners. `Done` is written before user code
                    // runs, so a panic drops each field once, as a local.
                    let (env, req, ready, call) = unsafe {
                        let fields = (
                            ptr::read(env),
                            ptr::read(req),
                            ptr::read(ready),
                            ptr::read(call),
                        );
                        ptr::write(state, State::Done);
                        fields
                    };
                    match ready(&env) {
                        None => (env, req, call),
                        Some(fut) => {
                            *state = State::Ready {
                                fut: Box::pin(fut),
                                env,
                                req,
                                call,
                            };
                            continue;
                        }
                    }
                }
                State::Ready { fut, .. } => {
                    let result = ready!(fut.as_mut().poll(cx));
                    let State::Ready { env, req, call, .. } = mem::replace(state, State::Done)
                    else {
                        unreachable!()
                    };
                    if let Err(err) = result {
                        return Poll::Ready(Err(err));
                    }
                    (env, req, call)
                }
                State::Done => panic!("`ReadyThenCall` polled after completion"),
            };
            *state = State::Call {
                fut: call(env, req),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::{cell::Cell, future::poll_fn, pin::pin, rc::Rc, task::Waker};

    use super::*;

    /// Counts drops of a value.
    struct Tracked(Rc<Cell<usize>>);

    impl Drop for Tracked {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    /// Readiness future that is pending `n` times, then resolves to `result`.
    fn wait(
        n: usize,
        result: Result<(), &'static str>,
    ) -> impl Future<Output = Result<(), &'static str>> {
        let mut left = n;
        poll_fn(move |cx| {
            if left == 0 {
                Poll::Ready(result)
            } else {
                left -= 1;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
    }

    async fn yield_once() {
        wait(1, Ok(())).await.unwrap();
    }

    fn poll_once<F: Future>(fut: Pin<&mut F>) -> Poll<F::Output> {
        fut.poll(&mut Context::from_waker(Waker::noop()))
    }

    fn complete<F: Future>(fut: F) -> F::Output {
        let mut fut = pin!(fut);
        loop {
            if let Poll::Ready(res) = poll_once(fut.as_mut()) {
                return res;
            }
        }
    }

    type Ready = std::future::Ready<Result<(), &'static str>>;

    /// STATEMENT: A ready scope skips the readiness wait and calls once with the
    /// shared environment and the request.
    ///
    /// HOW:
    /// - Return `None` from `ready` and count `ready` and `call` invocations.
    /// - Confirm the call result is built from `env` and `req`, and both
    ///   closures ran once.
    #[test]
    fn ready_scope_calls_directly() {
        let (readies, calls) = (Cell::new(0), Cell::new(0));
        let fut = ReadyThenCall::new(
            (10, &readies, &calls),
            5,
            |&(_, readies, _): &(usize, &Cell<usize>, &Cell<usize>)| {
                readies.set(readies.get() + 1);
                None::<Ready>
            },
            |(env, _, calls): (usize, &Cell<usize>, &Cell<usize>), req: usize| {
                calls.set(calls.get() + 1);
                async move { Ok::<_, &'static str>(env + req) }
            },
        );
        assert_eq!(complete(fut), Ok(15));
        assert_eq!((readies.get(), calls.get()), (1, 1));
    }

    /// STATEMENT: A not-ready scope waits for readiness before creating the call
    /// future, and the call then runs to completion.
    ///
    /// HOW:
    /// - Return a readiness future that is pending twice.
    /// - Confirm the first two polls are pending and `call` has not run.
    /// - Confirm the third poll creates the call, and a call future that
    ///   yields once completes on the fourth poll.
    #[test]
    fn waits_for_readiness_then_calls() {
        let calls = Cell::new(0);
        let fut = ReadyThenCall::new(
            &calls,
            7,
            |_: &&Cell<usize>| Some(wait(2, Ok(()))),
            |calls: &Cell<usize>, req: usize| {
                calls.set(calls.get() + 1);
                async move {
                    yield_once().await;
                    Ok::<_, &'static str>(req * 2)
                }
            },
        );
        let mut fut = pin!(fut);
        assert!(poll_once(fut.as_mut()).is_pending());
        assert!(poll_once(fut.as_mut()).is_pending());
        assert_eq!(calls.get(), 0);
        assert!(poll_once(fut.as_mut()).is_pending());
        assert_eq!(calls.get(), 1);
        assert_eq!(poll_once(fut.as_mut()), Poll::Ready(Ok(14)));
    }

    /// STATEMENT: A readiness error is returned without calling the service, and
    /// the environment and request are dropped once.
    ///
    /// HOW:
    /// - Return a readiness future that fails after one pending poll, with a
    ///   tracked `env` and request.
    /// - Confirm the error is returned, `call` never runs, and each tracked
    ///   value is dropped once before the future itself is dropped.
    #[test]
    fn readiness_error_skips_call() {
        let drops = Rc::new(Cell::new(0));
        let fut = ReadyThenCall::new(
            Tracked(drops.clone()),
            Tracked(drops.clone()),
            |_: &Tracked| Some(wait(1, Err("not ready"))),
            |_: Tracked, _: Tracked| -> std::future::Ready<Result<(), &'static str>> {
                panic!("called after a readiness error")
            },
        );
        let mut fut = pin!(fut);
        assert!(poll_once(fut.as_mut()).is_pending());
        assert_eq!(poll_once(fut.as_mut()), Poll::Ready(Err("not ready")));
        assert_eq!(drops.get(), 2);
    }

    /// STATEMENT: Polling after a readiness error panics instead of polling the
    /// completed readiness future.
    ///
    /// HOW:
    /// - Fail readiness on the first poll, then poll again under `catch_unwind`.
    /// - Confirm the second poll panics.
    #[test]
    fn poll_after_error_panics() {
        let fut = ReadyThenCall::new(
            (),
            (),
            |(): &()| Some(std::future::ready(Err("not ready"))),
            |(), ()| std::future::ready(Ok::<(), &'static str>(())),
        );
        let mut fut = pin!(fut);
        assert_eq!(poll_once(fut.as_mut()), Poll::Ready(Err("not ready")));
        let res = catch_unwind(AssertUnwindSafe(|| poll_once(fut.as_mut())));
        assert!(res.is_err());
    }

    /// Builds a call future that tracks drops of `env`, the request and both
    /// closures. The ready closure moves its guard into the readiness future,
    /// which is pending `ready_polls` times; `None` skips the wait. The call
    /// future holds `env` and the request and is pending once.
    fn tracked(
        drops: &Rc<Cell<usize>>,
        ready_polls: Option<usize>,
    ) -> impl Future<Output = Result<(), &'static str>> {
        let (ready_guard, call_guard) = (Tracked(drops.clone()), Tracked(drops.clone()));
        ReadyThenCall::new(
            Tracked(drops.clone()),
            Tracked(drops.clone()),
            move |_: &Tracked| {
                let guard = ready_guard;
                ready_polls.map(|n| async move {
                    let _guard = guard;
                    wait(n, Ok(())).await
                })
            },
            move |env: Tracked, req: Tracked| {
                let _guard = call_guard;
                async move {
                    let _held = (env, req);
                    yield_once().await;
                    Ok(())
                }
            },
        )
    }

    /// STATEMENT: Dropping the call future in any state drops every value it
    /// owns exactly once.
    ///
    /// HOW:
    /// - Build a call future with four tracked values: `env`, the request and
    ///   one guard in each closure.
    /// - Drop it unpolled, while readiness is pending, while the call is
    ///   pending, and after completion.
    /// - Confirm values already consumed are dropped when their step ends, and
    ///   all four are dropped once after the future is dropped.
    #[test]
    fn drops_each_value_once_in_every_state() {
        let drops = Rc::new(Cell::new(0));
        drop(tracked(&drops, Some(1)));
        assert_eq!(drops.get(), 4);

        // The ready guard lives in the pending readiness future.
        let drops = Rc::new(Cell::new(0));
        {
            let mut fut = pin!(tracked(&drops, Some(1)));
            assert!(poll_once(fut.as_mut()).is_pending());
            assert_eq!(drops.get(), 0);
        }
        assert_eq!(drops.get(), 4);

        // Readiness resolved and the call closure ran, so both guards are gone.
        let drops = Rc::new(Cell::new(0));
        {
            let mut fut = pin!(tracked(&drops, Some(0)));
            assert!(poll_once(fut.as_mut()).is_pending());
            assert_eq!(drops.get(), 2);
        }
        assert_eq!(drops.get(), 4);

        // The completed call future dropped `env` and the request.
        let drops = Rc::new(Cell::new(0));
        {
            let mut fut = pin!(tracked(&drops, None));
            assert!(poll_once(fut.as_mut()).is_pending());
            assert_eq!(drops.get(), 2);
            assert_eq!(poll_once(fut.as_mut()), Poll::Ready(Ok(())));
            assert_eq!(drops.get(), 4);
        }
        assert_eq!(drops.get(), 4);
    }

    /// STATEMENT: A panic in `ready`, in `call`, or while polling readiness drops
    /// every owned value once, and the future can then be dropped safely.
    ///
    /// HOW:
    /// - Panic from each step under `catch_unwind` with tracked `env` and
    ///   request.
    /// - Confirm the panic propagates, and each tracked value is dropped once
    ///   after the future is dropped; Miri checks for double drops and leaks.
    /// - Confirm a `ready` or `call` panic leaves the future done, so another
    ///   poll panics instead of reusing moved values.
    #[test]
    fn panics_drop_each_value_once() {
        fn check<F: Future>(drops: &Rc<Cell<usize>>, fut: F, polls: usize, done: bool) {
            {
                let mut fut = pin!(fut);
                for _ in 1..polls {
                    assert!(poll_once(fut.as_mut()).is_pending());
                }
                let res = catch_unwind(AssertUnwindSafe(|| poll_once(fut.as_mut())));
                assert!(res.is_err());
                if done {
                    let res = catch_unwind(AssertUnwindSafe(|| poll_once(fut.as_mut())));
                    assert!(res.is_err());
                }
            }
            assert_eq!(drops.get(), 2);
        }

        let drops = Rc::new(Cell::new(0));
        let fut = ReadyThenCall::new(
            Tracked(drops.clone()),
            Tracked(drops.clone()),
            |_: &Tracked| -> Option<Ready> { panic!("ready") },
            |_: Tracked, _: Tracked| std::future::ready(Ok::<(), &'static str>(())),
        );
        check(&drops, fut, 1, true);

        let drops = Rc::new(Cell::new(0));
        let fut = ReadyThenCall::new(
            Tracked(drops.clone()),
            Tracked(drops.clone()),
            |_: &Tracked| Some(wait(1, Ok(()))),
            |_: Tracked, _: Tracked| -> Ready { panic!("call") },
        );
        check(&drops, fut, 2, true);

        let drops = Rc::new(Cell::new(0));
        let fut = ReadyThenCall::new(
            Tracked(drops.clone()),
            Tracked(drops.clone()),
            |_: &Tracked| {
                Some(poll_fn(|_| -> Poll<Result<(), &'static str>> {
                    panic!("poll")
                }))
            },
            |_: Tracked, _: Tracked| std::future::ready(Ok::<(), &'static str>(())),
        );
        check(&drops, fut, 1, false);
    }

    /// STATEMENT: A self-referential call future stays valid across polls, and
    /// the environment and request move into it unchanged.
    ///
    /// HOW:
    /// - Wait for readiness, then call with an `async` block that borrows its
    ///   own local data across two yields.
    /// - Confirm the result is computed through the borrow; run under Miri to
    ///   check the borrow stays valid.
    #[test]
    fn self_referential_call_stays_valid() {
        let fut = ReadyThenCall::new(
            vec![1, 2, 3],
            vec![4, 5],
            |_: &Vec<u32>| Some(wait(1, Ok(()))),
            |env: Vec<u32>, req: Vec<u32>| async move {
                let all: Vec<u32> = env.into_iter().chain(req).collect();
                let slice = &all[1..];
                yield_once().await;
                let first = slice[0];
                yield_once().await;
                Ok::<_, &'static str>(first + slice.iter().sum::<u32>())
            },
        );
        assert_eq!(complete(fut), Ok(16));
    }
}
