//! QuickJS engine for rmcp-codemode: pi's sandbox lifecycle over rquickjs.
//!
//! The prelude is a verbatim port of pi's
//! `packages/codemode/src/runtime/prelude-source.ts`; the bridge transports
//! its messages over rquickjs instead of worker threads (script-invisible).

pub mod clock;
pub mod engine;
pub mod fingerprint;
pub mod prelude;

pub use engine::QuickJsEngine;
