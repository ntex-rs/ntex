use std::ptr;

use super::*;

/// Runs `f` and returns the message it panicked with.
fn panic_message<R>(f: impl FnOnce() -> R) -> String {
    let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .err()
        .expect("expected a panic");
    match err.downcast::<String>() {
        Ok(msg) => *msg,
        Err(err) => (*err.downcast::<&str>().unwrap()).to_string(),
    }
}

/// STATEMENT: Configuration cannot be retrieved while its shared context is being built.
///
/// HOW:
/// - Request the configuration from inside its context-assignment hook.
/// - Verify construction panics instead of exposing an unfinished configuration.
#[test]
#[allow(clippy::should_panic_without_expect)]
#[should_panic]
fn access_cfg_in_building_state() {
    #[derive(Debug)]
    struct TestCfg {
        config: CfgContext,
    }
    impl TestCfg {
        fn new() -> Self {
            Self {
                config: CfgContext::default(),
            }
        }
    }
    impl Default for TestCfg {
        fn default() -> Self {
            panic!()
        }
    }
    impl Configuration for TestCfg {
        const NAME: &str = "TEST";
        fn ctx(&self) -> &CfgContext {
            &self.config
        }
        fn set_ctx(&mut self, ctx: CfgContext) {
            let _ = ctx.shared().get::<TestCfg>();
            self.config = ctx;
        }
    }
    let _ = TestCfg::new().ctx();
    let _ = SharedCfg::new("TEST").add(TestCfg::new());
}

/// STATEMENT: Shared configuration preserves context identity through lookup, cloning, and
/// reassignment.
///
/// HOW:
/// - Build named and default contexts and retrieve typed configurations through their handles.
/// - Drop original handles, reassign a configuration handle, and check tags, service names, and
///   identities.
#[test]
fn shared_cfg() {
    #[derive(Default, Debug)]
    struct TestCfg {
        config: CfgContext,
    }
    impl Configuration for TestCfg {
        const NAME: &str = "TEST";
        fn ctx(&self) -> &CfgContext {
            &self.config
        }
        fn set_ctx(&mut self, ctx: CfgContext) {
            self.config = ctx;
        }
    }

    let cfg: SharedCfg = SharedCfg::new("TEST")
        .add(TestCfg::default())
        .service("SVC")
        .into();

    assert_eq!(cfg.tag(), "TEST");
    assert_eq!(cfg.service(), "SVC");
    let t = cfg.get::<TestCfg>();
    assert_eq!(t.tag(), "TEST");
    assert_eq!(t.service(), "SVC");
    assert_eq!(t.shared(), cfg);
    let t: Cfg<TestCfg> = Cfg::default();
    assert_eq!(t.tag(), "--");
    assert_eq!(t.service(), "");
    assert_eq!(t.ctx().id(), t.id());

    let t: Cfg<TestCfg> = t.ctx().get();
    assert_eq!(t.tag(), "--");
    assert_eq!(t.ctx().id(), t.id());

    let cfg = SharedCfg::new("TEST2").build();
    let cfg = SharedCfg::from(&cfg);
    let t = cfg.get::<TestCfg>();
    assert_eq!(t.tag(), "TEST2");
    assert_eq!(t.id(), cfg.id());
    drop(cfg);

    let cfg2 = t.shared();
    let mut t2 = cfg2.get::<TestCfg>();
    assert_eq!(t2.tag(), "TEST2");
    assert_eq!(t2.id(), cfg2.id());
    t2 = SharedCfg::from(SharedCfg::new("TEST3")).get::<TestCfg>();

    let cfg2 = t2.shared();
    let t3 = cfg2.get::<TestCfg>();
    assert_eq!(t3.tag(), "TEST3");
    assert_eq!(t3.id(), cfg2.id());

    let t = SharedCfg::from(SharedCfg::new("TEST4").add(TestCfg::default())).get::<TestCfg>();
    let cfg = t.shared();
    assert_eq!(t.id(), cfg.id());
    let t2 = t.clone();
    assert_eq!(t2.id(), cfg.id());
    assert_eq!(t2.tag(), "TEST4");

    let t3 = t.ctx().get::<TestCfg>();
    let t3 = t3.get_shared::<TestCfg>();
    assert_eq!(t3.id(), cfg.id());
    assert_eq!(t3.tag(), "TEST4");

    let t1 = SharedCfg::from(t3);
    assert_eq!(t.id(), t1.id());
}

/// STATEMENT: Configuration conversions retain values and shared identity determines hashing.
///
/// HOW:
/// - Convert owned configurations, builders, and shared handles into typed configurations.
/// - Check values and tags, then compare hashes of a shared handle, its clone, and its id.
#[test]
fn cfg_conversions() {
    use std::hash::BuildHasher;

    #[derive(Default, Debug)]
    struct TestCfg {
        config: CfgContext,
        val: usize,
    }
    impl Configuration for TestCfg {
        const NAME: &str = "TEST";
        fn ctx(&self) -> &CfgContext {
            &self.config
        }
        fn set_ctx(&mut self, ctx: CfgContext) {
            self.config = ctx;
        }
    }
    let new = |val| TestCfg {
        val,
        ..Default::default()
    };

    let t = Cfg::from(new(1));
    assert_eq!(t.val, 1);
    assert_eq!(t.tag(), "--");

    let shared = SharedCfg::from(new(2));
    assert_eq!(shared.get::<TestCfg>().val, 2);
    assert_eq!(Cfg::<TestCfg>::from(&shared).val, 2);
    assert_eq!(Cfg::<TestCfg>::from(shared.clone()).val, 2);

    let t: Cfg<TestCfg> = SharedCfg::new("T").add(new(3)).into();
    assert_eq!(t.val, 3);
    assert_eq!(t.tag(), "T");

    let hasher = foldhash::fast::FixedState::default();
    assert_eq!(hasher.hash_one(&shared), hasher.hash_one(shared.clone()));
    assert_eq!(hasher.hash_one(&shared), hasher.hash_one(shared.id()));
}

/// STATEMENT: A missing configuration falls back to a default cached per shared configuration
/// and thread, while an added value is the same instance on every thread.
///
/// HOW:
/// - Number each `TestCfg::default()` from a counter, so every generated default is distinct.
/// - Look up a missing `TestCfg` twice, through `SharedCfg::get` and `Cfg::get_shared`, and
///   check both return the first cached default.
/// - Look it up on another thread and in another `SharedCfg`, and check each creates a new
///   default.
/// - Add `TestCfg` explicitly and check another thread sees the added instance.
#[test]
fn missing_cfg_defaults_per_thread() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQ: AtomicUsize = AtomicUsize::new(1);

    #[derive(Debug)]
    struct TestCfg {
        config: CfgContext,
        val: usize,
    }
    impl Default for TestCfg {
        fn default() -> Self {
            Self {
                config: CfgContext::default(),
                val: SEQ.fetch_add(1, Ordering::SeqCst),
            }
        }
    }
    impl Configuration for TestCfg {
        const NAME: &str = "TEST";
        fn ctx(&self) -> &CfgContext {
            &self.config
        }
        fn set_ctx(&mut self, ctx: CfgContext) {
            self.config = ctx;
        }
    }

    #[derive(Debug, Default)]
    struct OtherCfg {
        config: CfgContext,
    }
    impl Configuration for OtherCfg {
        const NAME: &str = "OTHER";
        fn ctx(&self) -> &CfgContext {
            &self.config
        }
        fn set_ctx(&mut self, ctx: CfgContext) {
            self.config = ctx;
        }
    }

    // missing value: a default is created and cached on this thread
    let shared = SharedCfg::new("MISSING").add(OtherCfg::default()).build();
    let first = shared.get::<TestCfg>();
    assert_eq!(first.tag(), "MISSING");
    let cached = shared.get::<OtherCfg>().get_shared::<TestCfg>();
    assert!(ptr::eq(ptr::from_ref(&*first), ptr::from_ref(&*cached)));
    assert_eq!(cached.val, first.val);

    // another thread creates its own default
    let shared2 = shared.clone();
    let other = std::thread::spawn(move || {
        let a = shared2.get::<TestCfg>().val;
        let b = shared2.get::<TestCfg>().val;
        assert_eq!(a, b);
        a
    })
    .join()
    .unwrap();
    assert_ne!(other, first.val);
    assert_eq!(shared.get::<TestCfg>().val, first.val);

    // another shared configuration has its own cached default
    let separate = SharedCfg::new("SEPARATE").build();
    assert_ne!(separate.get::<TestCfg>().val, first.val);

    // explicit value: the same instance on every thread
    let explicit = SharedCfg::new("EXPLICIT")
        .add(TestCfg {
            config: CfgContext::default(),
            val: 0,
        })
        .build();
    let local = explicit.get::<TestCfg>();
    assert_eq!(local.val, 0);
    let local_addr = ptr::from_ref(&*local) as usize;
    let explicit2 = explicit.clone();
    let remote_addr = std::thread::spawn(move || {
        let cfg = explicit2.get::<TestCfg>();
        assert_eq!(cfg.val, 0);
        ptr::from_ref(&*cfg) as usize
    })
    .join()
    .unwrap();
    assert_eq!(local_addr, remote_addr);
}

/// STATEMENT: An owned value converts into a `Cfg` that keeps its settings, and a registered
/// value is reached through its context rather than a borrowed conversion.
///
/// HOW:
/// - Convert a standalone `TestCfg` with a non-default `val` into `Cfg` and check `val` and
///   the `--` tag survive.
/// - Register a non-default value in a `SharedCfg`, look it up again through
///   `value.ctx().get()`, and check it is the same instance with the same `val` and tag.
#[test]
fn owned_and_registered_cfg_keep_values() {
    #[derive(Default, Debug)]
    struct TestCfg {
        config: CfgContext,
        val: usize,
    }
    impl Configuration for TestCfg {
        const NAME: &str = "TEST";
        fn ctx(&self) -> &CfgContext {
            &self.config
        }
        fn set_ctx(&mut self, ctx: CfgContext) {
            self.config = ctx;
        }
    }

    let owned = Cfg::from(TestCfg {
        val: 42,
        ..Default::default()
    });
    assert_eq!(owned.val, 42);
    assert_eq!(owned.tag(), "--");

    let shared = SharedCfg::new("REGISTERED")
        .add(TestCfg {
            val: 7,
            ..Default::default()
        })
        .build();
    let registered = shared.get::<TestCfg>();
    let value: &TestCfg = &registered;
    let again = value.ctx().get::<TestCfg>();
    assert!(ptr::eq(ptr::from_ref(value), ptr::from_ref(&*again)));
    assert_eq!(again.val, 7);
    assert_eq!(again.tag(), "REGISTERED");
}

/// STATEMENT: A default context stays valid after the thread that created it exits, and every
/// thread shares the same default configuration.
///
/// HOW:
/// - Create `TestCfg::default()` on a spawned thread, return it, and join the thread.
/// - Read `tag()` and `id()` through the returned context and check they match
///   `SharedCfg::default()` on this thread.
/// - Look up a `Cfg` and a `SharedCfg` through the context and check they belong to the default
///   configuration. Miri reports a use-after-free if the default storage died with its thread.
#[test]
fn default_ctx_outlives_its_thread() {
    #[derive(Debug, Default)]
    struct TestCfg {
        config: CfgContext,
    }
    impl Configuration for TestCfg {
        const NAME: &str = "TEST";
        fn ctx(&self) -> &CfgContext {
            &self.config
        }
        fn set_ctx(&mut self, ctx: CfgContext) {
            self.config = ctx;
        }
    }

    let val = std::thread::spawn(TestCfg::default).join().unwrap();
    assert_eq!(val.ctx().tag(), "--");
    assert_eq!(val.ctx().id(), SharedCfg::default().id());

    let cfg = val.ctx().get::<TestCfg>();
    assert_eq!(cfg.tag(), "--");
    drop(cfg);
    assert_eq!(val.ctx().shared(), SharedCfg::default());
}

/// STATEMENT: A `Cfg` releases the configuration it was taken from, even when the value's
/// `set_ctx` ignores the context it is given.
///
/// HOW:
/// - Define `IgnoringCfg`, whose `set_ctx` keeps its default context instead of the one passed
///   in, and add it to the `OWNER` configuration.
/// - Repeatedly get, clone, and drop `Cfg` handles, checking each reports the `OWNER` tag.
/// - Check `OWNER` is left with only its own handle and the default configuration is still
///   usable. Miri reports a dangling reference if the drops released the default storage.
#[test]
fn cfg_drop_ignores_user_ctx() {
    #[derive(Debug, Default)]
    struct IgnoringCfg {
        config: CfgContext,
    }
    impl Configuration for IgnoringCfg {
        const NAME: &str = "IGNORING";
        fn ctx(&self) -> &CfgContext {
            &self.config
        }
        fn set_ctx(&mut self, _: CfgContext) {}
    }

    let shared = SharedCfg::new("OWNER").add(IgnoringCfg::default()).build();
    for _ in 0..4 {
        let cfg = shared.get::<IgnoringCfg>();
        let cloned = cfg.clone();
        assert_eq!(cfg.tag(), "OWNER");
        assert_eq!(cloned.tag(), "OWNER");
        assert!(ptr::eq(ptr::from_ref(&*cfg), ptr::from_ref(&*cloned)));
    }
    assert_eq!(Arc::strong_count(&shared.0), 1);
    assert_eq!(SharedCfg::default().tag(), "--");
    assert_eq!(Cfg::<IgnoringCfg>::default().tag(), "--");
}

/// STATEMENT: A context that escapes its configuration keeps reporting its id, tag, and service
/// name, while lookups through it panic once the configuration is unavailable.
///
/// HOW:
/// - Define `HandoffCfg`, whose `set_ctx` hands its context to the test through a shared slot.
/// - Add it to a builder, drop the builder, and check `get` and `shared` report the building
///   panic while `service` reports the tag.
/// - Add it to the built `ESCAPED` configuration and check the context reaches it. Drop the
///   configuration, then check `id`, `tag`, and `service` still work while `get` and `shared`
///   report that the configuration was dropped.
#[test]
fn escaped_ctx_outlives_cfg() {
    use std::sync::Mutex;

    type Slot = Arc<Mutex<Option<CfgContext>>>;

    #[derive(Debug, Default)]
    struct HandoffCfg {
        config: CfgContext,
        slot: Slot,
    }
    impl Configuration for HandoffCfg {
        const NAME: &str = "HANDOFF";
        fn ctx(&self) -> &CfgContext {
            &self.config
        }
        fn set_ctx(&mut self, ctx: CfgContext) {
            *self.slot.lock().unwrap() = Some(ctx);
        }
    }
    let handoff = |slot: &Slot| HandoffCfg {
        slot: slot.clone(),
        ..Default::default()
    };
    let slot = Slot::default();

    drop(
        SharedCfg::new("ABANDONED")
            .service("SVC")
            .add(handoff(&slot)),
    );
    let ctx = slot.lock().unwrap().take().unwrap();
    assert_eq!(ctx.tag(), "ABANDONED");
    assert_eq!(ctx.service(), "ABANDONED");
    let building = "ABANDONED: Cannot access shared config while building";
    assert_eq!(panic_message(|| ctx.get::<HandoffCfg>()), building);
    assert_eq!(panic_message(|| ctx.shared()), building);

    let shared = SharedCfg::new("ESCAPED")
        .add(handoff(&slot))
        .service("SVC")
        .build();
    let ctx = slot.lock().unwrap().take().unwrap();
    assert_eq!(ctx.shared(), shared);
    assert_eq!(ctx.get::<HandoffCfg>().id(), shared.id());
    let id = shared.id();
    drop(shared);

    assert_eq!(ctx.id(), id);
    assert_eq!(ctx.tag(), "ESCAPED");
    assert_eq!(ctx.service(), "SVC");
    let expired = "ESCAPED: Cannot access shared config after it was dropped";
    assert_eq!(panic_message(|| ctx.get::<HandoffCfg>()), expired);
    assert_eq!(panic_message(|| ctx.shared()), expired);
}

/// STATEMENT: While a configuration is being built, its context reports the tag as the service
/// name and refuses lookups; building publishes the final service name to every value, and a
/// later value of the same type replaces the earlier one.
///
/// HOW:
/// - Define `ProbeCfg`, whose `set_ctx` records the service name and the panic messages of
///   `get` and `shared` before keeping the context.
/// - Add `ProbeCfg` twice and `OtherCfg` once, setting the service name before and between
///   the adds.
/// - Check what each `set_ctx` recorded, check every value's context reports the final service
///   name after building, and check only the second `ProbeCfg` is left.
#[test]
fn builder_publishes_on_build() {
    use std::sync::Mutex;

    #[derive(Debug, Default)]
    struct ProbeCfg {
        config: CfgContext,
        val: usize,
        seen: Arc<Mutex<Vec<String>>>,
    }
    impl Configuration for ProbeCfg {
        const NAME: &str = "PROBE";
        fn ctx(&self) -> &CfgContext {
            &self.config
        }
        fn set_ctx(&mut self, ctx: CfgContext) {
            let get = panic_message(|| ctx.get::<ProbeCfg>());
            let shared = panic_message(|| ctx.shared());
            let service = ctx.service().to_string();
            self.seen.lock().unwrap().extend([service, get, shared]);
            self.config = ctx;
        }
    }

    #[derive(Debug, Default)]
    struct OtherCfg {
        config: CfgContext,
    }
    impl Configuration for OtherCfg {
        const NAME: &str = "OTHER";
        fn ctx(&self) -> &CfgContext {
            &self.config
        }
        fn set_ctx(&mut self, ctx: CfgContext) {
            self.config = ctx;
        }
    }

    let seen = Arc::new(Mutex::new(Vec::new()));
    let new_probe = |val| ProbeCfg {
        val,
        seen: seen.clone(),
        ..Default::default()
    };
    let shared = SharedCfg::new("BUILDING")
        .service("EARLY")
        .add(new_probe(1))
        .add(OtherCfg::default())
        .service("FINAL")
        .add(new_probe(2))
        .build();

    let building = "BUILDING: Cannot access shared config while building";
    let step = ["BUILDING", building, building].map(String::from);
    assert_eq!(*seen.lock().unwrap(), [step.clone(), step].concat());

    assert_eq!(shared.service(), "FINAL");
    let probe = shared.get::<ProbeCfg>();
    assert_eq!(probe.val, 2);
    assert_eq!(probe.service(), "FINAL");
    assert_eq!(probe.ctx().service(), "FINAL");
    assert_eq!(probe.ctx().shared(), shared);
    assert_eq!(shared.get::<OtherCfg>().ctx().service(), "FINAL");
    // the replaced first value released its handle to `seen`
    assert_eq!(Arc::strong_count(&seen), 2);
}

/// STATEMENT: A panic in `Default` or `set_ctx` leaves existing handles and the fallback cache
/// usable, and every value it created is dropped exactly once.
///
/// HOW:
/// - Count live `CountedCfg` values; its `Default` and `set_ctx` panic while their flags are set.
/// - On a spawned thread, make each hook panic while creating the missing `CountedCfg` default,
///   through a registered value's context and through `SharedCfg::get`. Then make `set_ctx`
///   panic while a builder already holds a `CountedCfg`.
/// - Check the configuration's strong count is unchanged and its handles still read their value.
/// - Clear the flags and look up `NestingCfg`, whose fallback `set_ctx` looks up the missing
///   `CountedCfg`, and check one `CountedCfg` default is cached. After the thread exits, check
///   no `CountedCfg` is alive.
#[test]
fn panicking_default_or_set_ctx_keeps_handles() {
    use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

    static LIVE: AtomicIsize = AtomicIsize::new(0);
    static FAIL_DEFAULT: AtomicBool = AtomicBool::new(false);
    static FAIL_SET_CTX: AtomicBool = AtomicBool::new(false);

    #[derive(Debug)]
    struct CountedCfg {
        config: CfgContext,
    }
    impl Default for CountedCfg {
        fn default() -> Self {
            assert!(!FAIL_DEFAULT.load(Ordering::SeqCst), "Default failed");
            LIVE.fetch_add(1, Ordering::SeqCst);
            Self {
                config: CfgContext::default(),
            }
        }
    }
    impl Drop for CountedCfg {
        fn drop(&mut self) {
            LIVE.fetch_sub(1, Ordering::SeqCst);
        }
    }
    impl Configuration for CountedCfg {
        const NAME: &str = "COUNTED";
        fn ctx(&self) -> &CfgContext {
            &self.config
        }
        fn set_ctx(&mut self, ctx: CfgContext) {
            assert!(!FAIL_SET_CTX.load(Ordering::SeqCst), "set_ctx failed");
            self.config = ctx;
        }
    }

    #[derive(Debug, Default)]
    struct OwnerCfg {
        config: CfgContext,
        val: usize,
    }
    impl Configuration for OwnerCfg {
        const NAME: &str = "OWNER";
        fn ctx(&self) -> &CfgContext {
            &self.config
        }
        fn set_ctx(&mut self, ctx: CfgContext) {
            self.config = ctx;
        }
    }

    #[derive(Debug, Default)]
    struct NestingCfg {
        config: CfgContext,
    }
    impl Configuration for NestingCfg {
        const NAME: &str = "NESTING";
        fn ctx(&self) -> &CfgContext {
            &self.config
        }
        fn set_ctx(&mut self, ctx: CfgContext) {
            assert_eq!(ctx.get::<CountedCfg>().ctx().id(), ctx.id());
            self.config = ctx;
        }
    }

    std::thread::spawn(|| {
        let shared = SharedCfg::new("PANIC")
            .add(OwnerCfg {
                val: 7,
                ..Default::default()
            })
            .build();
        let owner = shared.get::<OwnerCfg>();
        let strong = Arc::strong_count(&shared.0);

        for (flag, msg) in [
            (&FAIL_DEFAULT, "Default failed"),
            (&FAIL_SET_CTX, "set_ctx failed"),
        ] {
            flag.store(true, Ordering::SeqCst);
            for _ in 0..3 {
                assert_eq!(panic_message(|| owner.ctx().get::<CountedCfg>()), msg);
                assert_eq!(panic_message(|| shared.get::<CountedCfg>()), msg);
            }
            flag.store(false, Ordering::SeqCst);
        }

        let builder = SharedCfg::new("BUILDER").add(CountedCfg::default());
        FAIL_SET_CTX.store(true, Ordering::SeqCst);
        let msg = panic_message(move || builder.add(CountedCfg::default()));
        FAIL_SET_CTX.store(false, Ordering::SeqCst);
        assert_eq!(msg, "set_ctx failed");

        assert_eq!(LIVE.load(Ordering::SeqCst), 0);
        assert_eq!(Arc::strong_count(&shared.0), strong);
        assert_eq!(owner.val, 7);
        assert_eq!(owner.ctx().get::<OwnerCfg>().val, 7);

        assert_eq!(owner.get_shared::<NestingCfg>().tag(), "PANIC");
        assert_eq!(LIVE.load(Ordering::SeqCst), 1);
        let counted = shared.get::<CountedCfg>();
        assert_eq!(counted.ctx().id(), shared.id());
        assert_eq!(LIVE.load(Ordering::SeqCst), 1);
    })
    .join()
    .unwrap();
    assert_eq!(LIVE.load(Ordering::SeqCst), 0);
}

/// STATEMENT: A fallback default cached for a configuration that was dropped can still read its
/// context's metadata when its thread exits, and the cache does not keep the configuration's
/// registered values alive.
///
/// HOW:
/// - On a spawned thread, build `CACHED` with service `SVC` and a registered `RegisteredCfg`
///   that counts its drops, then look up the missing `ReportingCfg` to cache its default.
/// - Drop every handle and check the registered value is dropped while the thread still runs.
/// - Let the thread exit; `ReportingCfg::drop` records its context's `id`, `tag`, and
///   `service`, and the test checks they still describe `CACHED`.
#[test]
fn cached_default_outlives_its_cfg() {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static DROPPED: AtomicUsize = AtomicUsize::new(0);
    static REPORT: Mutex<Vec<(usize, &str, &str)>> = Mutex::new(Vec::new());

    #[derive(Debug, Default)]
    struct RegisteredCfg {
        config: CfgContext,
    }
    impl Drop for RegisteredCfg {
        fn drop(&mut self) {
            DROPPED.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl Configuration for RegisteredCfg {
        const NAME: &str = "REGISTERED";
        fn ctx(&self) -> &CfgContext {
            &self.config
        }
        fn set_ctx(&mut self, ctx: CfgContext) {
            self.config = ctx;
        }
    }

    #[derive(Debug, Default)]
    struct ReportingCfg {
        config: CfgContext,
    }
    impl Drop for ReportingCfg {
        fn drop(&mut self) {
            let ctx = &self.config;
            let report = (ctx.id(), ctx.tag(), ctx.service());
            REPORT.lock().unwrap().push(report);
        }
    }
    impl Configuration for ReportingCfg {
        const NAME: &str = "REPORTING";
        fn ctx(&self) -> &CfgContext {
            &self.config
        }
        fn set_ctx(&mut self, ctx: CfgContext) {
            self.config = ctx;
        }
    }

    let id = std::thread::spawn(|| {
        let shared = SharedCfg::new("CACHED")
            .service("SVC")
            .add(RegisteredCfg::default())
            .build();
        let id = shared.id();
        let cached = shared.get::<ReportingCfg>();
        assert_eq!(cached.ctx().id(), id);
        drop(cached);
        drop(shared);
        assert_eq!(DROPPED.load(Ordering::SeqCst), 1);
        assert!(REPORT.lock().unwrap().is_empty());
        id
    })
    .join()
    .unwrap();
    assert_eq!(*REPORT.lock().unwrap(), [(id, "CACHED", "SVC")]);
}
