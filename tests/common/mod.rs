/// Run the same behavioral tests against a packaged or installed release.
/// CI sets this explicitly; local tests use Cargo's freshly built binary.
/// A missing binary or bash fails the test instead of skipping it, so a
/// wrong `CMDQ_TEST_BINARY` cannot turn the suite into silent passes.
pub fn cmdq_binary() -> std::ffi::OsString {
    let bin =
        std::env::var_os("CMDQ_TEST_BINARY").unwrap_or_else(|| env!("CARGO_BIN_EXE_cmdq").into());
    assert!(
        std::path::Path::new(&bin).exists(),
        "cmdq binary not found: {}",
        std::path::Path::new(&bin).display()
    );
    assert!(
        std::path::Path::new("/bin/bash").exists(),
        "/bin/bash is required by the PTY tests"
    );
    bin
}
