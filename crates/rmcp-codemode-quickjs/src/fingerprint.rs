//! The VM fingerprint (SPEC §5.5, pi `runtime/vm.ts:316-319`): reported before
//! the first call of every deterministic run; a recorded run (host-side
//! suspend record) must only be resumed against the same fingerprint.

use crate::{clock::fnv1a, prelude::PRELUDE_SOURCE};

/// Manual version of the determinism semantics. Bump whenever a
/// replay-visible behavior changes (PRNG algorithm, UTC date behavior,
/// result application order) — the pinned-value tests guard it. (pi:
/// `DETERMINISTIC_VERSION = 1`, `vm.ts:69`.)
pub const DETERMINISTIC_VERSION: u64 = 1;

/// The engine identity slot. Bump when the QuickJS runtime underneath changes
/// (rquickjs/libqjs upgrade) — behaviorally equivalent to pi's
/// `quickjs-wasi …, quickjs …` slots, collapsed into one because rquickjs
/// does not expose the embedded QuickJS version string.
pub const ENGINE_VERSION: &str = "rquickjs/0.14";

/// The default QuickJS stack size, mirroring pi's `MAX_STACK_SIZE` (512 KiB,
/// `vm.ts:55-58`).
pub const MAX_STACK_SIZE: usize = 512 * 1024;

/// The fingerprint string, pi's slot list mapped to our stack. `budget` is
/// always `none`: deadline preemption uses the interrupt handler without
/// counting polls (pi's in-thread backend behaves the same,
/// `deterministic.test.ts:207`).
pub fn fingerprint(memory_limit_bytes: Option<u64>) -> String {
    let heap = memory_limit_bytes
        .map(|bytes| bytes.to_string())
        .unwrap_or_else(|| "none".to_owned());
    format!(
        "engine {}, prelude {}, deterministic {}, stack {}, heap {}, budget none",
        ENGINE_VERSION,
        fnv1a(PRELUDE_SOURCE),
        DETERMINISTIC_VERSION,
        MAX_STACK_SIZE,
        heap,
    )
}
