//! TwinSync core: one-way "Update Copy" from a Mac folder to two external drives.

#[cfg(not(target_os = "macos"))]
compile_error!(
    "TwinSync targets macOS: the transfer layer uses renameatx_np, F_FULLFSYNC and diskutil"
);

pub mod app;
pub mod config;
pub mod devices;
pub mod domain;
pub mod engine;
pub mod errors;
pub mod monitor;
pub mod planner;
pub mod platform;
pub mod scanner;
pub mod storage;
pub mod transfer;
pub mod ui;
