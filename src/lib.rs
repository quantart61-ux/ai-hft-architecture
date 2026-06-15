//! Agent Risk OS - Ultra-Low Latency IPC and Constraint Execution Framework
//!
//! High-performance trading risk constraint engine with zero-GC, lock-free primitives.
//!
//! # Architecture
//!
//! Three-layer stack:
//! 1. **IPC Layer** (ipc_mmap): Memory-mapped files for Python<->Rust zero-copy constraint exchange
//! 2. **Queue Layer** (spsc_queue): Lock-free SPSC ring buffer for constraint notifications
//! 3. **Execution Layer** (constraint_engine): Deterministic risk limit application and breach detection
//!
//! # Features
//!
//! - `@audit:zero-allocation`: No dynamic heap allocation in critical path
//! - `@audit:lock-free`: Atomic operations only; no mutexes, spinlocks, or condition variables
//! - `@audit:cache-aligned`: Cache-line padding prevents false sharing between producer/consumer
//! - `@audit:zero-copy`: Direct buffer parsing without intermediate serialization
//!
//! # Example
//!
//! ```ignore\n//! use ai_hft_architecture::critical_path::{ConstraintEngine, RiskConstraintPayload};\n//!\n//! let mut engine = ConstraintEngine::new();\n//!\n//! let constraint = RiskConstraintPayload {\n//!     constraint_seq_id: 1,\n//!     max_var_limit: 1_000_000.0,\n//!     max_position_concentration: 0.25,\n//!     max_intraday_loss: 500_000.0,\n//!     max_leverage: 3.5,\n//!     delta_hedge_target: 0.0,\n//!     vega_hedge_target: 0.0,\n//!     constraint_timestamp_ns: 1_000_000_000,\n//!     reserved_a: [0; 32],\n//! };\n//!\n//! engine.apply_constraint(\n//!     &constraint,\n//!     50_000.0,      // current_pnl\n//!     600_000.0,     // current_notional\n//!     2.5,           // current_leverage\n//!     0.0,           // current_delta\n//!     0.0,           // current_vega\n//!     0.18,          // current_max_concentration\n//! ).expect(\"constraint application failed\");\n//! ```\n\npub mod critical_path;\n\npub use critical_path::{\n    ConstraintEngine, ConstraintError, ConstraintResult, ConstraintSeqId,\n    PortfolioState, PortfolioRiskMetric, RiskBreachType, RiskConstraintPayload,\n    MmapProducer, MmapConsumer, MmapError, MmapFileHeader, MmapHandle,\n    SpscRingBuffer,\n};\n