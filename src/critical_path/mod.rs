//! Critical Path Module: Ultra-low latency IPC, queue, and constraint execution
//!
//! Exports core primitives for Agent Risk OS integration:
//! - ipc_mmap: Memory-mapped file protocol for Python<->Rust payload exchange
//! - spsc_queue: Lock-free SPSC ring buffer for notification signalling
//! - constraint_engine: Deterministic risk constraint execution kernel

pub mod ipc_mmap;
pub mod spsc_queue;
pub mod constraint_engine;

pub use constraint_engine::{
    ConstraintEngine, ConstraintError, ConstraintResult, ConstraintSeqId,
    PortfolioState, PortfolioRiskMetric, RiskBreachType, RiskConstraintPayload,
};

pub use ipc_mmap::{
    MmapProducer, MmapConsumer, MmapError, MmapFileHeader, MmapHandle,
};

pub use spsc_queue::SpscRingBuffer;
