//! Multi-tier state management: L1 DashMap, L2 mmap, L3 RocksDB.
//!
//! Provides:
//! - `L1Store`: in-memory DashMap (fastest, volatile)
//! - Typed wrappers: `ValueState`, `MapState`, `ListState`, `CounterState`
//! - Windowing: `TumblingWindows`, `SlidingWindows`, `SessionWindows`
//! - Watermark tracking and late event handling

pub mod l1;
#[cfg(feature = "mmap")]
pub mod l2;
pub mod l3;
pub mod tiered;
pub mod typed;
pub mod window;

pub use l1::L1Store;
#[cfg(feature = "mmap")]
pub use l2::L2Store;
pub use l3::{BatchEntry, BatchOp, KvPairs, L3Backend, L3Store};
#[cfg(feature = "redb")]
pub use l3::{RedbConfig, RedbStore};
pub use tiered::{TieredConfig, TieredStore};
pub use typed::{CounterState, ListState, MapState, ValueState};
pub use window::{
    LatePolicy, SessionEvent, SessionTracker, SessionWindows, SlidingWindows, TumblingWindows,
    Watermark, Window,
};
