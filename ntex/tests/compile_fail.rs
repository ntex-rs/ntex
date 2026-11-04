/// STATEMENT: Common web handler, extractor, responder, error, and state mistakes
/// produce the documented diagnostics.
///
/// HOW:
/// - Compile each case in `tests/ui` with `trybuild`.
/// - Compare the compiler output with the matching `.stderr` snapshot.
///
/// Diagnostics change between compiler releases, so CI runs this test only on
/// stable. Update the snapshots with
/// `TRYBUILD=overwrite cargo +stable test -p ntex --features tokio --test compile_fail -- --ignored`.
#[test]
#[ignore = "compiler diagnostics differ between toolchains; run on stable"]
fn compile_fail() {
    trybuild::TestCases::new().compile_fail("tests/ui/*.rs");
}
