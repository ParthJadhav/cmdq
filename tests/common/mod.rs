/// Run the same behavioral tests against a packaged or installed release.
/// CI sets this explicitly; local tests use Cargo's freshly built binary.
pub fn cmdq_binary() -> std::ffi::OsString {
    std::env::var_os("CMDQ_TEST_BINARY").unwrap_or_else(|| env!("CARGO_BIN_EXE_cmdq").into())
}
