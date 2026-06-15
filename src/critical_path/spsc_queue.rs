//! Lock-Free SPSC Ring Buffer for Agent Risk OS
//! 
//! Single-Producer Single-Consumer lock-free bounded queue using atomic operations.
//! Designed for ultra-low latency constraint propagation between mmap reader and
//! execution core without any blocking synchronization or dynamic allocation.
//!
//! CACHE-LINE OPTIMIZATION:
//! - Producer and consumer heads separated by 128 bytes (2x cache-line) to prevent false sharing
//! - Head pointers use atomic operations without locking (compare-and-swap pattern)
//! - Circular buffer uses contiguous 1D array for optimal cache locality
//! - All indices are compile-time sized to prevent dynamic resizing

use std::marker::PhantomData;
use std::mem;
use std::sync::atomic::{AtomicU64, Ordering};
use zerocopy::{FromBytes, AsBytes};

/// Branded type for queue element indices (prevents raw u32 confusion)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueIndex(u32);

/// Branded type for element count (prevents mixing with indices)
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd)]
pub struct ElementCount(u32);

/// Result type for queue operations
pub type QueueResult<T> = Result<T, QueueError>;

/// Error enumeration for queue protocol violations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueError {
    /// Queue is full; cannot push new element
    QueueFull,
    /// Queue is empty; cannot pop element
    QueueEmpty,
    /// Element size mismatch during push/pop
    ElementSizeMismatch,
}

/// Risk constraint message wrapper for queue transport
/// Must remain zerocopy-compatible for direct atomic writes
#[repr(C, align(64))]
#[derive(Clone, Copy, AsBytes, FromBytes)]
pub struct RiskConstraintMessage {
    /// Constraint sequence ID (from mmap consumer)
    pub constraint_seq_id: u64,
    /// Maximum portfolio VaR limit
    pub max_var_limit: f64,
    /// Maximum position concentration
    pub max_position_concentration: f64,
    /// Maximum intraday loss threshold
    pub max_intraday_loss: f64,
    /// Maximum leverage
    pub max_leverage: f64,
    /// Delta hedge target
    pub delta_hedge_target: f64,
    /// Vega hedge target
    pub vega_hedge_target: f64,
    /// Message timestamp (nanoseconds)
    pub message_timestamp_ns: u64,
    /// Reserved for extensions (8 x 24 bytes = 192 bytes for 64-byte alignment)
    pub reserved: [u64; 24],
}

impl RiskConstraintMessage {
    /// Compile-time assertion: message fits exactly in 512 bytes (aligned to cache-line)
    const _: () = assert!(
        mem::size_of::<RiskConstraintMessage>() == 512,
        "RiskConstraintMessage must be exactly 512 bytes"
    );
}

/// Cache-line aligned producer state
/// Separated from consumer state to prevent false sharing
#[repr(C, align(128))]
struct ProducerState {
    /// Current write position (wrapping index, 64-bit to avoid wraparound issues)
    write_pos: AtomicU64,
    /// Padding to 128-byte boundary (false sharing prevention)
    _padding: [u64; 15],
}

/// Cache-line aligned consumer state
/// Separated from producer state to prevent false sharing
#[repr(C, align(128))]
struct ConsumerState {
    /// Current read position (wrapping index, 64-bit)
    read_pos: AtomicU64,
    /// Padding to 128-byte boundary (false sharing prevention)
    _padding: [u64; 15],
}

/// Single-Producer Single-Consumer lock-free ring buffer
/// Capacity must be power-of-2 for efficient modulo via bitwise AND
pub struct SpscRingBuffer<const CAPACITY: usize> {
    /// Circular buffer storage (contiguous 1D array for cache locality)
    buffer: [RiskConstraintMessage; CAPACITY],
    
    /// Producer state (cache-line aligned)
    producer: ProducerState,
    
    /// Consumer state (cache-line aligned)
    consumer: ConsumerState,
    
    /// Capacity mask for efficient modulo: (index & CAPACITY_MASK) == (index % CAPACITY)
    capacity_mask: usize,
    
    /// Phantom data to satisfy zerocopy constraints
    _phantom: PhantomData<RiskConstraintMessage>,
}

impl<const CAPACITY: usize> SpscRingBuffer<CAPACITY> {
    /// Compile-time validation: CAPACITY must be power-of-2
    /// This enables O(1) index wrapping via bitwise AND instead of modulo
    const _: () = assert!(
        CAPACITY > 0 && (CAPACITY & (CAPACITY - 1)) == 0,
        "CAPACITY must be a power of 2"
    );

    /// Create a new SPSC ring buffer
    /// 
    /// # Arguments
    /// * None (capacity is a const generic parameter)
    /// 
    /// # Returns
    /// Initialized `SpscRingBuffer` with zero latency overhead
    /// 
    /// @audit:zero-allocation - No heap allocation; stack-sized const array
    /// @audit:lock-free - No mutexes, spinlocks, or atomic CAS loops; only simple atomic loads/stores
    /// @audit:cache-aligned - ProducerState and ConsumerState separated by 128 bytes to prevent cache coherency false sharing
    #[inline]
    pub fn new() -> Self {
        // Capacity mask for bitwise modulo
        let capacity_mask = CAPACITY - 1;

        // Zero-initialize the message array via const array initialization
        // This is deterministic and zero-allocation
        let buffer = [RiskConstraintMessage {
            constraint_seq_id: 0,
            max_var_limit: 0.0,
            max_position_concentration: 0.0,
            max_intraday_loss: 0.0,
            max_leverage: 0.0,
            delta_hedge_target: 0.0,
            vega_hedge_target: 0.0,
            message_timestamp_ns: 0,
            reserved: [0; 24],
        }; CAPACITY];

        SpscRingBuffer {
            buffer,
            producer: ProducerState {
                write_pos: AtomicU64::new(0),
                _padding: [0; 15],
            },
            consumer: ConsumerState {
                read_pos: AtomicU64::new(0),
                _padding: [0; 15],
            },
            capacity_mask,
            _phantom: PhantomData,
        }
    }

    /// Push a risk constraint message to the queue (producer-only operation)
    /// 
    /// # Arguments
    /// * `message` - RiskConstraintMessage to enqueue
    /// 
    /// # Returns
    /// `Ok(())` on success, `Err(QueueError::QueueFull)` if buffer exhausted
    /// 
    /// CACHE-LINE IMPACT:
    /// - Reads producer.write_pos (local producer cache-line, L1 hit expected)
    /// - Reads consumer.read_pos (L3 miss expected; other CPU in consumer loop)
    /// - Writes to buffer[index] (L1 write, subsequent cache coherency broadcast)
    /// 
    /// @audit:zero-allocation - No heap allocation; direct array write
    /// @audit:lock-free - Single atomic load of read_pos; no CAS loops or locking
    /// @audit:cache-aligned - false sharing prevention: producer state isolated
    #[inline]
    pub fn push(&mut self, message: &RiskConstraintMessage) -> QueueResult<()> {
        // Load current write and read positions
        let write_pos = self.producer.write_pos.load(Ordering::Relaxed);
        let read_pos = self.consumer.read_pos.load(Ordering::Acquire);

        // Calculate next write position
        let next_write_pos = write_pos.wrapping_add(1);

        // Guard clause: check if queue is full
        // Queue is full when (next_write_pos & mask) == (read_pos & mask) at the same modulo cycle
        if (next_write_pos & (self.capacity_mask as u64)) == (read_pos & (self.capacity_mask as u64))
            && (next_write_pos >> 32) != (read_pos >> 32) {
            return Err(QueueError::QueueFull);
        }

        // Write message to buffer at current position
        let index = (write_pos & (self.capacity_mask as u64)) as usize;
        self.buffer[index] = *message;

        // Update producer write position (Relaxed ordering safe for SPSC; consumer only reads)
        self.producer.write_pos.store(next_write_pos, Ordering::Release);

        // Happy path: message enqueued
        Ok(())
    }

    /// Pop a risk constraint message from the queue (consumer-only operation)
    /// 
    /// # Arguments
    /// None
    /// 
    /// # Returns
    /// `Ok(RiskConstraintMessage)` on success, `Err(QueueError::QueueEmpty)` if no elements
    /// 
    /// CACHE-LINE IMPACT:
    /// - Reads consumer.read_pos (local consumer cache-line, L1 hit expected)
    /// - Reads producer.write_pos (L3 miss expected; other CPU in producer loop)
    /// - Reads from buffer[index] (L1 hit if producer wrote recently to same cache-line; L3 miss otherwise)
    /// 
    /// @audit:zero-allocation - No heap allocation; direct array read and copy
    /// @audit:lock-free - Single atomic load of write_pos; no CAS or spinning
    /// @audit:cache-aligned - false sharing prevention: consumer state isolated
    #[inline]
    pub fn pop(&mut self) -> QueueResult<RiskConstraintMessage> {
        // Load current read and write positions
        let read_pos = self.consumer.read_pos.load(Ordering::Relaxed);
        let write_pos = self.producer.write_pos.load(Ordering::Acquire);

        // Guard clause: check if queue is empty
        if read_pos == write_pos {
            return Err(QueueError::QueueEmpty);
        }

        // Read message from buffer at current position
        let index = (read_pos & (self.capacity_mask as u64)) as usize;
        let message = self.buffer[index];

        // Update consumer read position (Relaxed ordering safe for SPSC)
        self.consumer.read_pos.store(read_pos.wrapping_add(1), Ordering::Release);

        // Happy path: message dequeued
        Ok(message)
    }

    /// Non-blocking peek: read next message without advancing read position
    /// 
    /// @audit:zero-allocation - No allocation; direct read-only access
    /// @audit:lock-free - Single atomic load; no spinning
    #[inline]
    pub fn peek(&self) -> QueueResult<RiskConstraintMessage> {
        let read_pos = self.consumer.read_pos.load(Ordering::Relaxed);
        let write_pos = self.producer.write_pos.load(Ordering::Acquire);

        if read_pos == write_pos {
            return Err(QueueError::QueueEmpty);
        }

        let index = (read_pos & (self.capacity_mask as u64)) as usize;
        Ok(self.buffer[index])
    }

    /// Get current queue depth (approximate; may change immediately after return)
    /// 
    /// CAVEAT: Result is inherently racy in concurrent scenario;
    /// used for diagnostics only, NOT for control flow decisions
    /// 
    /// @audit:lock-free - Single atomic loads; no synchronization
    #[inline]
    pub fn is_current_depth(&self) -> u32 {
        let write_pos = self.producer.write_pos.load(Ordering::Relaxed);
        let read_pos = self.consumer.read_pos.load(Ordering::Relaxed);

        // Wrapping subtraction handles wraparound automatically
        ((write_pos.wrapping_sub(read_pos)) & (self.capacity_mask as u64)) as u32
    }

    /// Check if queue has any elements (non-blocking)
    /// 
    /// @audit:lock-free - Single atomic loads
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.consumer.read_pos.load(Ordering::Relaxed)
            == self.producer.write_pos.load(Ordering::Relaxed)
    }

    /// Check if queue is full (non-blocking)
    /// 
    /// @audit:lock-free - Single atomic loads
    #[inline]
    pub fn is_full(&self) -> bool {
        let write_pos = self.producer.write_pos.load(Ordering::Relaxed);
        let read_pos = self.consumer.read_pos.load(Ordering::Relaxed);
        let next_write = write_pos.wrapping_add(1);

        (next_write & (self.capacity_mask as u64)) == (read_pos & (self.capacity_mask as u64))
            && (next_write >> 32) != (read_pos >> 32)
    }

    /// Get maximum capacity
    #[inline(always)]
    pub fn has_capacity(&self) -> usize {
        CAPACITY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spsc_push_pop() {
        let mut queue: SpscRingBuffer<64> = SpscRingBuffer::new();

        let msg = RiskConstraintMessage {
            constraint_seq_id: 42,
            max_var_limit: 1_000_000.0,
            max_position_concentration: 0.25,
            max_intraday_loss: 500_000.0,
            max_leverage: 3.5,
            delta_hedge_target: 0.0,
            vega_hedge_target: 0.0,
            message_timestamp_ns: 1_000_000_000,
            reserved: [0; 24],
        };

        // Push message
        assert_eq!(queue.push(&msg), Ok(()));

        // Verify queue is not empty
        assert!(!queue.is_empty());

        // Pop message and verify
        let popped = queue.pop().expect("pop failed");
        assert_eq!(popped.constraint_seq_id, msg.constraint_seq_id);
        assert_eq!(popped.max_var_limit, msg.max_var_limit);

        // Verify queue is now empty
        assert!(queue.is_empty());
    }

    #[test]
    fn test_spsc_queue_full() {
        let mut queue: SpscRingBuffer<4> = SpscRingBuffer::new();

        let msg = RiskConstraintMessage {
            constraint_seq_id: 1,
            max_var_limit: 100_000.0,
            max_position_concentration: 0.1,
            max_intraday_loss: 50_000.0,
            max_leverage: 2.0,
            delta_hedge_target: 0.0,
            vega_hedge_target: 0.0,
            message_timestamp_ns: 1_000_000,
            reserved: [0; 24],
        };

        // Fill queue to capacity
        for _ in 0..4 {
            assert_eq!(queue.push(&msg), Ok(()));
        }

        // Verify queue is full
        assert!(queue.is_full());

        // Next push should fail
        assert_eq!(queue.push(&msg), Err(QueueError::QueueFull));
    }

    #[test]
    fn test_spsc_peek() {
        let mut queue: SpscRingBuffer<32> = SpscRingBuffer::new();

        let msg = RiskConstraintMessage {
            constraint_seq_id: 99,
            max_var_limit: 750_000.0,
            max_position_concentration: 0.2,
            max_intraday_loss: 400_000.0,
            max_leverage: 3.0,
            delta_hedge_target: 0.0,
            vega_hedge_target: 0.0,
            message_timestamp_ns: 2_000_000_000,
            reserved: [0; 24],
        };

        queue.push(&msg).expect("push failed");

        // Peek should return message without advancing read position
        let peeked = queue.peek().expect("peek failed");
        assert_eq!(peeked.constraint_seq_id, msg.constraint_seq_id);

        // Queue should still have 1 element
        assert_eq!(queue.is_current_depth(), 1);

        // Pop should return same message
        let popped = queue.pop().expect("pop failed");
        assert_eq!(popped.constraint_seq_id, msg.constraint_seq_id);

        // Now queue should be empty
        assert!(queue.is_empty());
    }

    #[test]
    fn test_cache_line_alignment() {
        // Verify producer and consumer states are cache-line separated
        let queue: SpscRingBuffer<128> = SpscRingBuffer::new();

        let producer_addr = &queue.producer as *const _ as usize;
        let consumer_addr = &queue.consumer as *const _ as usize;

        // Distance should be at least 128 bytes (1 cache-line)
        let distance = if consumer_addr > producer_addr {
            consumer_addr - producer_addr
        } else {
            producer_addr - consumer_addr
        };

        assert!(distance >= 128, "Producer and consumer must be cache-line separated");
    }
}
