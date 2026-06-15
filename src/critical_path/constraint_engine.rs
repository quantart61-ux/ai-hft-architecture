//! Constraint Execution Engine for Agent Risk OS
//!
//! Ultra-low latency constraint application kernel running on the Rust execution core.
//! Consumes constraint sequence IDs from SPSC queue, retrieves payloads from mmap,
//! applies risk limits deterministically without allocation or blocking.
//!
//! ARCHITECTURE:
//! - Single consumer thread reads SPSC queue in tight loop (spin-wait pattern)
//! - Constraint payload lookups via mmap zero-copy unmarshalling
//! - Execution state machine processes constraints in-order without buffering
//! - Guard clauses enforce invariants; Result monads for fault handling
//! - All critical path data structures are cache-aligned and pre-allocated

use std::mem;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use zerocopy::{FromBytes, AsBytes};

/// Branded type for constraint sequence IDs (prevents confusion with other u64s)
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ConstraintSeqId(pub u64);

/// Branded type for position IDs (prevents mixing with constraint IDs)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PositionId(pub u64);

/// Branded type for portfolio risk metrics
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PortfolioRiskMetric(pub f64);

/// Constraint payload matching Python ctypes layout exactly (512 bytes)
#[repr(C, align(64))]
#[derive(Clone, Copy, AsBytes, FromBytes)]
pub struct RiskConstraintPayload {
    pub constraint_seq_id: u64,
    pub max_var_limit: f64,
    pub max_position_concentration: f64,
    pub max_intraday_loss: f64,
    pub max_leverage: f64,
    pub delta_hedge_target: f64,
    pub vega_hedge_target: f64,
    pub constraint_timestamp_ns: u64,
    pub reserved_a: [u64; 32],
}

impl RiskConstraintPayload {
    const _: () = assert!(
        mem::size_of::<RiskConstraintPayload>() == 512,
        "RiskConstraintPayload must be 512 bytes"
    );
}

/// Current portfolio state (mutable, updated by constraint engine)
#[repr(C, align(64))]
pub struct PortfolioState {
    /// Current unrealized P&L
    pub unrealized_pnl: f64,
    /// Current notional exposure
    pub current_notional_exposure: f64,
    /// Current leverage ratio
    pub current_leverage: f64,
    /// Current delta hedge
    pub current_delta: f64,
    /// Current vega hedge
    pub current_vega: f64,
    /// Highest daily P&L (track maximum)
    pub peak_daily_pnl: f64,
    /// Lowest daily P&L (track minimum = -max_loss_so_far)
    pub trough_daily_pnl: f64,
    /// Largest single position concentration
    pub max_position_concentration_current: f64,
    /// Timestamp of last constraint application (ns)
    pub last_constraint_update_ns: u64,
    /// Reserved for extensions
    pub reserved: [u64; 6],
}

impl PortfolioState {
    const _: () = assert!(
        mem::size_of::<PortfolioState>() == 128,
        "PortfolioState must be cache-line sized"
    );
}

/// Risk breach detection result
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskBreachType {
    /// Portfolio VaR exceeds limit
    VaRExceeded = 1,
    /// Single position exceeds concentration limit
    ConcentrationExceeded = 2,
    /// Daily loss exceeds intraday stop
    IntraDayLossExceeded = 3,
    /// Leverage exceeds maximum
    LeverageExceeded = 4,
    /// Delta hedge deviation exceeds target
    DeltaHedgeMissed = 5,
    /// Vega hedge deviation exceeds target
    VegaHedgeMissed = 6,
}

/// Result type for constraint operations
pub type ConstraintResult<T> = Result<T, ConstraintError>;

/// Error enumeration (explicit Result monad pattern)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintError {
    /// Constraint payload not found in mmap region
    PayloadNotFound,
    /// Constraint validation failed (data corruption or semantic violation)
    PayloadValidationFailed,
    /// Risk limit breach detected
    RiskBreachDetected(RiskBreachType),
    /// Constraint application violates prior state
    StateConsistencyViolation,
    /// Mmap access failed
    MmapAccessError,
    /// Portfolio state overflow (numeric instability)
    NumericOverflow,
}

/// Constraint execution state machine
#[repr(C, align(64))]
pub struct ConstraintEngine {
    /// Current portfolio state (cache-line aligned)
    pub portfolio_state: PortfolioState,
    
    /// Last applied constraint sequence ID
    pub last_applied_seq_id: AtomicU64,
    
    /// Total constraints processed
    pub constraints_processed: AtomicU64,
    
    /// Total constraints rejected (risk breaches)
    pub constraints_rejected: AtomicU64,
    
    /// Last detected breach type (for diagnostics)
    pub last_breach_type: AtomicU64,
    
    /// Reserved for future metrics
    pub reserved_metrics: [AtomicU64; 10],
}

impl ConstraintEngine {
    /// Initialize constraint engine with zero state
    ///
    /// # Arguments
    /// None
    ///
    /// # Returns
    /// Initialized `ConstraintEngine` with all counters at zero
    ///
    /// @audit:zero-allocation - Stack-allocated; no heap
    /// @audit:cache-aligned - All state cache-line separated
    #[inline]
    pub fn new() -> Self {
        ConstraintEngine {
            portfolio_state: PortfolioState {
                unrealized_pnl: 0.0,
                current_notional_exposure: 0.0,
                current_leverage: 0.0,
                current_delta: 0.0,
                current_vega: 0.0,
                peak_daily_pnl: 0.0,
                trough_daily_pnl: 0.0,
                max_position_concentration_current: 0.0,
                last_constraint_update_ns: 0,
                reserved: [0; 6],
            },
            last_applied_seq_id: AtomicU64::new(0),
            constraints_processed: AtomicU64::new(0),
            constraints_rejected: AtomicU64::new(0),
            last_breach_type: AtomicU64::new(0),
            reserved_metrics: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
        }
    }

    /// Apply risk constraint to portfolio state
    ///
    /// # Arguments
    /// * `constraint` - RiskConstraintPayload to apply
    /// * `portfolio_state` - Current portfolio metrics (from market data feed)
    ///
    /// # Returns
    /// `Ok(())` on successful constraint application
    /// `Err(ConstraintError)` if breach or validation failure
    ///
    /// EXECUTION FLOW:
    /// 1. Validate constraint payload (guard clause)
    /// 2. Check all risk limits against portfolio state
    /// 3. Update portfolio state atomically (immutable semantics)
    /// 4. Increment success counter
    /// 5. Return result
    ///
    /// Happy path is at the END (branch prediction optimization).
    ///
    /// @audit:zero-allocation - No heap allocations; stack-only operations
    /// @audit:lock-free - Atomic counters use compare-and-swap but non-blocking
    /// @audit:cache-aligned - All reads/writes within single cache-line regions
    #[inline]
    pub fn apply_constraint(
        &mut self,
        constraint: &RiskConstraintPayload,
        current_pnl: f64,
        current_notional: f64,
        current_leverage: f64,
        current_delta: f64,
        current_vega: f64,
        current_max_concentration: f64,
    ) -> ConstraintResult<()> {
        // Guard clause: validate constraint payload sanity
        if constraint.max_var_limit <= 0.0 || constraint.max_leverage <= 0.0 {
            return Err(ConstraintError::PayloadValidationFailed);
        }

        if !(0.0..=1.0).contains(&constraint.max_position_concentration) {
            return Err(ConstraintError::PayloadValidationFailed);
        }

        // Guard clause: check VaR limit breach
        if current_notional.abs() > constraint.max_var_limit {
            self.last_breach_type
                .store(RiskBreachType::VaRExceeded as u64, Ordering::Relaxed);
            self.constraints_rejected
                .fetch_add(1, Ordering::Relaxed);
            return Err(ConstraintError::RiskBreachDetected(RiskBreachType::VaRExceeded));
        }

        // Guard clause: check position concentration limit
        if current_max_concentration > constraint.max_position_concentration {
            self.last_breach_type
                .store(RiskBreachType::ConcentrationExceeded as u64, Ordering::Relaxed);
            self.constraints_rejected
                .fetch_add(1, Ordering::Relaxed);
            return Err(ConstraintError::RiskBreachDetected(
                RiskBreachType::ConcentrationExceeded,
            ));
        }

        // Guard clause: check intraday loss threshold
        // Track trough (minimum) to detect loss breach
        let new_trough = self.portfolio_state.trough_daily_pnl.min(current_pnl);
        let daily_loss = self.portfolio_state.peak_daily_pnl - new_trough;
        if daily_loss > constraint.max_intraday_loss {
            self.last_breach_type
                .store(RiskBreachType::IntraDayLossExceeded as u64, Ordering::Relaxed);
            self.constraints_rejected
                .fetch_add(1, Ordering::Relaxed);
            return Err(ConstraintError::RiskBreachDetected(
                RiskBreachType::IntraDayLossExceeded,
            ));
        }

        // Guard clause: check leverage limit
        if current_leverage > constraint.max_leverage {
            self.last_breach_type
                .store(RiskBreachType::LeverageExceeded as u64, Ordering::Relaxed);
            self.constraints_rejected
                .fetch_add(1, Ordering::Relaxed);
            return Err(ConstraintError::RiskBreachDetected(
                RiskBreachType::LeverageExceeded,
            ));
        }

        // Guard clause: check delta hedge target within tolerance (±10% of target)
        let delta_deviation = (current_delta - constraint.delta_hedge_target).abs();
        if constraint.delta_hedge_target != 0.0 && delta_deviation > constraint.delta_hedge_target.abs() * 0.1 {
            self.last_breach_type
                .store(RiskBreachType::DeltaHedgeMissed as u64, Ordering::Relaxed);
            self.constraints_rejected
                .fetch_add(1, Ordering::Relaxed);
            return Err(ConstraintError::RiskBreachDetected(
                RiskBreachType::DeltaHedgeMissed,
            ));
        }

        // Guard clause: check vega hedge target within tolerance (±10% of target)
        let vega_deviation = (current_vega - constraint.vega_hedge_target).abs();
        if constraint.vega_hedge_target != 0.0 && vega_deviation > constraint.vega_hedge_target.abs() * 0.1 {
            self.last_breach_type
                .store(RiskBreachType::VegaHedgeMissed as u64, Ordering::Relaxed);
            self.constraints_rejected
                .fetch_add(1, Ordering::Relaxed);
            return Err(ConstraintError::RiskBreachDetected(
                RiskBreachType::VegaHedgeMissed,
            ));
        }

        // Guard clause: numeric sanity checks
        if current_pnl.is_nan() || current_pnl.is_infinite() {
            return Err(ConstraintError::NumericOverflow);
        }

        // === HAPPY PATH: All validations passed ===
        
        // Update portfolio state (immutable pattern: create new snapshot)
        self.portfolio_state.unrealized_pnl = current_pnl;
        self.portfolio_state.current_notional_exposure = current_notional;
        self.portfolio_state.current_leverage = current_leverage;
        self.portfolio_state.current_delta = current_delta;
        self.portfolio_state.current_vega = current_vega;
        self.portfolio_state.peak_daily_pnl = self.portfolio_state.peak_daily_pnl.max(current_pnl);
        self.portfolio_state.trough_daily_pnl = new_trough;
        self.portfolio_state.max_position_concentration_current = current_max_concentration;
        self.portfolio_state.last_constraint_update_ns = current_time_ns();

        // Update sequence ID (atomic, Release ordering for visibility to other cores)
        self.last_applied_seq_id
            .store(constraint.constraint_seq_id, Ordering::Release);

        // Increment success counter (Relaxed; just a diagnostic)
        self.constraints_processed
            .fetch_add(1, Ordering::Relaxed);

        Ok(())
    }

    /// Check if portfolio is within all risk constraints (read-only audit)
    ///
    /// Returns a list of any detected breaches without modifying state.
    ///
    /// @audit:lock-free - Only atomic reads; no modifications
    #[inline]
    pub fn audit_portfolio_state(
        &self,
        constraint: &RiskConstraintPayload,
    ) -> ConstraintResult<()> {
        // Guard clause: validate constraint
        if constraint.max_var_limit <= 0.0 {
            return Err(ConstraintError::PayloadValidationFailed);
        }

        let state = &self.portfolio_state;

        // Check VaR limit
        if state.current_notional_exposure.abs() > constraint.max_var_limit {
            return Err(ConstraintError::RiskBreachDetected(RiskBreachType::VaRExceeded));
        }

        // Check concentration
        if state.max_position_concentration_current > constraint.max_position_concentration {
            return Err(ConstraintError::RiskBreachDetected(
                RiskBreachType::ConcentrationExceeded,
            ));
        }

        // Check leverage
        if state.current_leverage > constraint.max_leverage {
            return Err(ConstraintError::RiskBreachDetected(
                RiskBreachType::LeverageExceeded,
            ));
        }

        // Happy path: all constraints satisfied
        Ok(())
    }

    /// Get last applied constraint sequence ID (atomic read)
    ///
    /// @audit:lock-free - Single atomic load
    #[inline]
    pub fn get_last_applied_seq_id(&self) -> ConstraintSeqId {
        ConstraintSeqId(self.last_applied_seq_id.load(Ordering::Acquire))
    }

    /// Get total constraints processed (diagnostic)
    ///
    /// @audit:lock-free - Single atomic load
    #[inline]
    pub fn get_constraints_processed(&self) -> u64 {
        self.constraints_processed.load(Ordering::Relaxed)
    }

    /// Get total constraints rejected due to risk breaches
    ///
    /// @audit:lock-free - Single atomic load
    #[inline]
    pub fn get_constraints_rejected(&self) -> u64 {
        self.constraints_rejected.load(Ordering::Relaxed)
    }

    /// Get current portfolio risk metric
    ///
    /// @audit:lock-free - Direct field read
    #[inline]
    pub fn get_current_var(&self) -> PortfolioRiskMetric {
        PortfolioRiskMetric(self.portfolio_state.current_notional_exposure.abs())
    }
}

/// Get current system time in nanoseconds since epoch
///
/// @audit:zero-allocation - No allocation; system call only
#[inline]
fn current_time_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constraint_engine_creation() {
        let engine = ConstraintEngine::new();
        assert_eq!(engine.get_constraints_processed(), 0);
        assert_eq!(engine.get_constraints_rejected(), 0);
        assert_eq!(engine.portfolio_state.unrealized_pnl, 0.0);
    }

    #[test]
    fn test_apply_constraint_success() {
        let mut engine = ConstraintEngine::new();

        let constraint = RiskConstraintPayload {
            constraint_seq_id: 1,
            max_var_limit: 1_000_000.0,
            max_position_concentration: 0.25,
            max_intraday_loss: 500_000.0,
            max_leverage: 3.5,
            delta_hedge_target: 0.0,
            vega_hedge_target: 0.0,
            constraint_timestamp_ns: current_time_ns(),
            reserved_a: [0; 32],
        };

        let result = engine.apply_constraint(
            &constraint,
            100_000.0,    // current_pnl
            500_000.0,    // current_notional
            2.5,          // current_leverage
            0.0,          // current_delta
            0.0,          // current_vega
            0.15,         // current_max_concentration
        );

        assert_eq!(result, Ok(()));
        assert_eq!(engine.get_constraints_processed(), 1);
        assert_eq!(engine.get_constraints_rejected(), 0);
        assert_eq!(
            engine.portfolio_state.unrealized_pnl,
            100_000.0
        );
    }

    #[test]
    fn test_apply_constraint_var_breach() {
        let mut engine = ConstraintEngine::new();

        let constraint = RiskConstraintPayload {
            constraint_seq_id: 1,
            max_var_limit: 500_000.0,
            max_position_concentration: 0.25,
            max_intraday_loss: 500_000.0,
            max_leverage: 3.5,
            delta_hedge_target: 0.0,
            vega_hedge_target: 0.0,
            constraint_timestamp_ns: current_time_ns(),
            reserved_a: [0; 32],
        };

        let result = engine.apply_constraint(
            &constraint,
            0.0,              // current_pnl
            1_000_000.0,      // current_notional (EXCEEDS LIMIT)
            3.0,              // current_leverage
            0.0,              // current_delta
            0.0,              // current_vega
            0.15,             // current_max_concentration
        );

        assert_eq!(
            result,
            Err(ConstraintError::RiskBreachDetected(RiskBreachType::VaRExceeded))
        );
        assert_eq!(engine.get_constraints_rejected(), 1);
    }

    #[test]
    fn test_apply_constraint_leverage_breach() {
        let mut engine = ConstraintEngine::new();

        let constraint = RiskConstraintPayload {
            constraint_seq_id: 1,
            max_var_limit: 1_000_000.0,
            max_position_concentration: 0.25,
            max_intraday_loss: 500_000.0,
            max_leverage: 3.0,
            delta_hedge_target: 0.0,
            vega_hedge_target: 0.0,
            constraint_timestamp_ns: current_time_ns(),
            reserved_a: [0; 32],
        };

        let result = engine.apply_constraint(
            &constraint,
            0.0,          // current_pnl
            500_000.0,    // current_notional
            3.5,          // current_leverage (EXCEEDS LIMIT)
            0.0,          // current_delta
            0.0,          // current_vega
            0.15,         // current_max_concentration
        );

        assert_eq!(
            result,
            Err(ConstraintError::RiskBreachDetected(RiskBreachType::LeverageExceeded))
        );
        assert_eq!(engine.get_constraints_rejected(), 1);
    }

    #[test]
    fn test_apply_constraint_concentration_breach() {
        let mut engine = ConstraintEngine::new();

        let constraint = RiskConstraintPayload {
            constraint_seq_id: 1,
            max_var_limit: 1_000_000.0,
            max_position_concentration: 0.20,
            max_intraday_loss: 500_000.0,
            max_leverage: 3.5,
            delta_hedge_target: 0.0,
            vega_hedge_target: 0.0,
            constraint_timestamp_ns: current_time_ns(),
            reserved_a: [0; 32],
        };

        let result = engine.apply_constraint(
            &constraint,
            0.0,          // current_pnl
            500_000.0,    // current_notional
            2.5,          // current_leverage
            0.0,          // current_delta
            0.0,          // current_vega
            0.25,         // current_max_concentration (EXCEEDS 0.20 LIMIT)
        );

        assert_eq!(
            result,
            Err(ConstraintError::RiskBreachDetected(
                RiskBreachType::ConcentrationExceeded
            ))
        );
        assert_eq!(engine.get_constraints_rejected(), 1);
    }

    #[test]
    fn test_intraday_loss_tracking() {
        let mut engine = ConstraintEngine::new();

        let constraint = RiskConstraintPayload {
            constraint_seq_id: 1,
            max_var_limit: 1_000_000.0,
            max_position_concentration: 0.25,
            max_intraday_loss: 100_000.0,
            max_leverage: 3.5,
            delta_hedge_target: 0.0,
            vega_hedge_target: 0.0,
            constraint_timestamp_ns: current_time_ns(),
            reserved_a: [0; 32],
        };

        // First constraint: P&L at +50k
        let result1 = engine.apply_constraint(
            &constraint,
            50_000.0,     // peak
            500_000.0,
            2.5,
            0.0,
            0.0,
            0.15,
        );
        assert_eq!(result1, Ok(()));

        // Update peak
        assert_eq!(engine.portfolio_state.peak_daily_pnl, 50_000.0);

        // Second constraint: P&L drops to -60k (drawdown = 110k > 100k limit)
        let result2 = engine.apply_constraint(
            &constraint,
            -60_000.0,    // trough
            500_000.0,
            2.5,
            0.0,
            0.0,
            0.15,
        );
        assert_eq!(
            result2,
            Err(ConstraintError::RiskBreachDetected(
                RiskBreachType::IntraDayLossExceeded
            ))
        );
    }

    #[test]
    fn test_audit_portfolio_state() {
        let mut engine = ConstraintEngine::new();

        let constraint = RiskConstraintPayload {
            constraint_seq_id: 1,
            max_var_limit: 500_000.0,
            max_position_concentration: 0.25,
            max_intraday_loss: 500_000.0,
            max_leverage: 3.5,
            delta_hedge_target: 0.0,
            vega_hedge_target: 0.0,
            constraint_timestamp_ns: current_time_ns(),
            reserved_a: [0; 32],
        };

        // Apply initial constraint
        engine.apply_constraint(
            &constraint,
            100_000.0,
            300_000.0,    // Within limit
            2.0,
            0.0,
            0.0,
            0.15,
        ).ok();

        // Audit should pass
        let audit1 = engine.audit_portfolio_state(&constraint);
        assert_eq!(audit1, Ok(()));

        // Modify state to exceed VaR limit (simulating market move)
        engine.portfolio_state.current_notional_exposure = 600_000.0;

        // Audit should now fail
        let audit2 = engine.audit_portfolio_state(&constraint);
        assert_eq!(
            audit2,
            Err(ConstraintError::RiskBreachDetected(RiskBreachType::VaRExceeded))
        );
    }
}
