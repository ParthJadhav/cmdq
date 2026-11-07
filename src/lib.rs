//! Library facade for cmdq — exposed so integration tests in the `tests/`
//! directory and external tooling can use the same modules the binary uses.

pub mod app;
mod atomic_file;
pub mod config;
mod cursor_tracker;
mod input;
mod mode_detect;
pub mod osc133;
mod panel;
pub(crate) mod paths;
mod pty;
pub mod queue;
pub mod session_lease;
pub mod shell_integration;
#[cfg(unix)]
mod terminal_input;
mod text_edit;
