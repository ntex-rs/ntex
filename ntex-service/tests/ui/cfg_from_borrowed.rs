use ntex_service::cfg::{Cfg, CfgContext, Configuration};

#[derive(Debug, Default)]
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

/// STATEMENT: A borrowed configuration value does not convert into `Cfg`, so its settings
/// cannot be silently replaced by a registry lookup.
///
/// HOW:
/// - Build a standalone `TestCfg` with a non-default value.
/// - Convert `&TestCfg` into `Cfg<TestCfg>` and expect a missing `From<&TestCfg>` impl.
fn main() {
    let value = TestCfg {
        val: 42,
        ..Default::default()
    };
    let _cfg: Cfg<TestCfg> = Cfg::from(&value);
}
