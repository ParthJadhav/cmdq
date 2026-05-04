//! Library facade for cmdq — exposed so integration tests in the `tests/`
//! directory and external tooling can use the same modules the binary uses.

pub mod app;
pub mod cursor_tracker;
pub mod input;
pub mod mode_detect;
pub mod osc133;
pub mod panel;
pub(crate) mod paths;
pub mod pty;
pub mod queue;
pub mod session_lease;
pub mod shell_integration;
