use super::*;

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

/// STATEMENT: Shared configuration preserves context identity through lookup, cloning, and replacement.
///
/// HOW:
/// - Build named and default contexts and retrieve typed configurations through their handles.
/// - Drop original handles, replace a configuration, and check tags, service names, and identities.
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
    let t2 = cfg2.get::<TestCfg>();
    assert_eq!(t2.tag(), "TEST2");
    assert_eq!(t2.id(), cfg2.id());
    unsafe { t2.replace(SharedCfg::from(SharedCfg::new("TEST3")).get::<TestCfg>()) };

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
    let t3 = Cfg::from(t3.get_ref());
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
