//! Library facade for cmdq — exposed so integration tests in the `tests/`
//! directory and external tooling can use the same modules the binary uses.

pub mod app;
pub mod input;
pub mod osc133;
pub mod pty;
pub mod queue;
pub mod shell_integration;
pub mod ui;
