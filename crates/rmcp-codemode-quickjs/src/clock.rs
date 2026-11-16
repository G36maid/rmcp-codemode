//! Determinism primitives (SPEC §5.4): the virtual clock and mulberry32.
//! (pi: `runtime/vm.ts:83-103`.)

use std::sync::Mutex;

/// The VM clock: starts at `started_at`, advances only on delivered results,
/// never backward. (pi: `VirtualClock`.)
#[derive(Debug)]
pub struct VirtualClock {
    state: Mutex<ClockState>,
}

#[derive(Debug)]
struct ClockState {
    started_at: u64,
    current: u64,
}

impl VirtualClock {
    pub fn new(started_at: u64) -> Self {
        Self {
            state: Mutex::new(ClockState {
                started_at,
                current: started_at,
            }),
        }
    }

    /// `Date.now()` until the first result arrives.
    pub fn now(&self) -> u64 {
        self.state.lock().unwrap().current
    }

    /// `performance.now()` counts from `started_at`.
    pub fn elapsed(&self) -> u64 {
        let state = self.state.lock().unwrap();
        state.current - state.started_at
    }

    /// While the VM processes result `seq`, the clock reads `now`, or the
    /// previous value if `now` is earlier (monotonic).
    pub fn advance_to(&self, now: u64) {
        let mut state = self.state.lock().unwrap();
        if now > state.current {
            state.current = now;
        }
    }

    pub fn started_at(&self) -> u64 {
        self.state.lock().unwrap().started_at
    }
}

/// mulberry32: a small, fast PRNG; one 32-bit draw per call. Ported
/// line-for-line from pi's `prng` (`runtime/vm.ts:83-96`) so draw sequences
/// match the oracle.
#[derive(Debug)]
pub struct Mulberry32 {
    state: Mutex<u32>,
}

impl Mulberry32 {
    pub fn new(seed: u32) -> Self {
        Self {
            state: Mutex::new(seed),
        }
    }

    /// Returns a f64 in `[0, 1)`, identical to pi's draw sequence.
    pub fn next_f64(&self) -> f64 {
        let mut state = self.state.lock().unwrap();
        // state = (state + 0x6d2b79f5) >>> 0
        *state = state.wrapping_add(0x6d2b_79f5);
        let s = *state;
        // value = imul(state ^ (state >>> 15), 1 | state)
        let mut value = (s ^ (s >> 15)).wrapping_mul(1 | s);
        // value = (value + imul(value ^ (value >>> 7), 61 | value)) ^ value
        value = (value.wrapping_add((value ^ (value >> 7)).wrapping_mul(61 | value))) ^ value;
        // return (value ^ (value >>> 14)) >>> 0
        let draw = value ^ (value >> 14);
        draw as f64 / 4294967296.0
    }
}

/// FNV-1a 32-bit, 8 lowercase hex chars. (pi: `runtime/vm.ts:72-79`.)
pub fn fnv1a(text: &str) -> String {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in text.chars() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{hash:08x}")
}
