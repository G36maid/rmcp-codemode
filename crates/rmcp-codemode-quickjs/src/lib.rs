//! The QuickJS engine for rmcp-codemode.
//!
//! Implements the core's [`Engine`] and [`SuspendingEngine`] contracts with
//! rquickjs: one fresh VM per run, the script as an async function body, and
//! the tool space as a namespaced `tools` proxy.
//!
//! Lifecycle clauses (the contract in `rmcp_codemode::engine`):
//! - One fresh VM per run; nothing leaks between runs. The VM — job queue
//!   included — drops when the run ends, cancelling unawaited calls.
//! - A run waiting on a promise nothing can settle fails immediately.
//! - The deadline preempts pure-JS loops through the VM's interrupt handler.
//! - `exit()` ends the run successfully even when a try/catch swallows it.
//! - Output caps fail the run, keeping partial output.

#![deny(missing_docs)]

pub mod engine;
mod proxy;
mod run;
pub mod server;

pub use engine::{QuickJsEngine, SuspendingQuickJsEngine};
pub use server::{
    CodemodeServer, FileStorePersister, NativeTool, ServerMode, Store, StorePersister,
};
