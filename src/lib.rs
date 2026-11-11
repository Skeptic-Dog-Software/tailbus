//! acrbus: adversarial convergence review (ACR) on an agent bus. A bus on
//! this machine carries messages between AI agents and their bridges to
//! headless agents, and `acr` reviews changes with them. A program built on
//! it, such as tailbus, names itself and adds networks through
//! `config::App`, then runs `cli::main`. See DESIGN.md.

mod acr;
mod alert;
mod ask;
mod attach;
mod bridge;
mod bus;
mod call;
pub mod cli;
pub mod config;
mod daemon;
mod format;
mod hook;
mod local;
mod mcp;
mod net;
pub mod network;
mod permission;
mod proto;
mod run_usage;
mod setup;
mod store;
mod usage;
mod util;
