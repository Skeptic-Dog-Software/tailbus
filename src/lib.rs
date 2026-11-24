//! tailbus: direct messages between AI agents, queued until they're read.
//! A bus on each machine carries messages between agents and their bridges
//! to headless agents, and between machines on a tailnet. `main` names the
//! program and its networks through `config::App`, then runs `cli::main`.

mod alert;
mod ask;
mod attach;
mod bridge;
mod bus;
mod call;
mod cleanup;
pub mod cli;
mod compat;
pub mod config;
mod daemon;
mod door;
mod format;
mod hook;
mod local;
mod mcp;
mod net;
pub mod network;
mod permission;
mod proto;
mod refresh;
mod run_usage;
mod setup;
mod store;
mod usage;
mod util;
