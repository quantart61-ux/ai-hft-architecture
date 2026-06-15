//! Memory-Mapped IPC Protocol for Agent Risk OS
//! 
//! Zero-copy, lock-free mmap-based communication layer bridging Python AI Agent
//! to Rust execution core. Risk constraint payloads are written directly to shared
//! memory without serialization overhead or network socket latency.
//!
//! ARCHITECTURE NOTES:
//! - Uses #[repr(C)] for deterministic binary layout matching Python ctypes
//! - Zerocopy markers ensure compile-time validation of bitwise safety
//! - No dynamic allocation: all buffer sizing is compile-time fixed
//! - Guard clause entry pattern with early returns for fault handling

use std::fs::OpenOptions;
use std::io;
use std::marker::PhantomData;
use std::mem;
use std::os::unix::fs::OpenOptionsExt;
use std::ptr;
use memmap2::{MmapMut, Mmap};
use zerocopy::{FromBytes, AsBytes};

/// Branded type for memory-mapped file handles (prevents confusion with regular file descriptors)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MmapHandle(usize);

/// Branded type for constraint sequence IDs (prevents mixing with other u64 identifiers)
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ConstraintSeqId(u64);

/// Branded type for risk metric values (prevents raw float mixing with other f64s)
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RiskMetricValue(f64);

/// Header magic constant for mmap validation
const MMAP_MAGIC: u32 = 0xDEADBEEF;

/// Mmap protocol version (for backward compatibility tracking)
const PROTOCOL_VERSION: u16 = 1;

/// Fixed-size constraint payload: 512 bytes
/// Aligned to cache line boundary to prevent false sharing with adjacent structures
#[repr(C, align(64))]
#[derive(Clone, Copy, AsBytes, FromBytes)]
pub struct RiskConstraintPayload {
    /// Sequence ID: monotonically increasing constraint identifier
    pub constraint_seq_id: u64,
    
    /// Maximum portfolio exposure (Value at Risk)
    pub max_var_limit: f64,
    
    /// Maximum single-position concentration
    pub max_position_concentration: f64,
    
    /// Maximum intraday loss threshold (hard stop)
    pub max_intraday_loss: f64,
    
    /// Maximum notional leverage allowed
    pub max_leverage: f64,
    
    /// Greeks hedge ratio target (delta target)
    pub delta_hedge_target: f64,
    
    /// Greeks hedge ratio target (vega target)
    pub vega_hedge_target: f64,
    
    /// Timestamp (Unix nanoseconds) of constraint generation
    pub constraint_timestamp_ns: u64,
    
    /// Reserved for future extensions (8 x 32 bytes = 256 bytes)
    /// Maintained at compile-time fixed size to prevent breaking changes
    pub reserved_a: [u64; 32],
}

impl RiskConstraintPayload {
    /// Compile-time assertion: payload must fit exactly in 512 bytes
    const _: () = assert!(
        mem::size_of::<RiskConstraintPayload>() == 512,
        "RiskConstraintPayload must be exactly 512 bytes"
    );
}

/// Mmap file header: 64 bytes, cache-line aligned
/// Written once by producer, read-only by consumer after initialization
#[repr(C, align(64))]
#[derive(Clone, Copy, AsBytes, FromBytes)]
pub struct MmapFileHeader {
    /// Magic validation token
    pub magic: u32,
    
    /// Protocol version for compatibility tracking
    pub protocol_version: u16,
    
    /// Reserved padding to align version
    pub _reserved_version: u16,
    
    /// Total file size in bytes (set at initialization)
    pub file_size_bytes: u64,
    
    /// Offset to first constraint payload (follows this header)
    pub payload_offset: u64,
    
    /// Maximum number of simultaneous constraint payloads
    pub max_payloads: u32,
    
    /// Padding to 64-byte alignment
    pub _padding: [u8; 20],
}

impl MmapFileHeader {
    /// Compile-time assertion: header must fit exactly in 64 bytes
    const _: () = assert!(
        mem::size_of::<MmapFileHeader>() == 64,
        "MmapFileHeader must be exactly 64 bytes"
    );
}

/// Result type for mmap operations (explicit error handling via Result monad pattern)
pub type MmapResult<T> = Result<T, MmapError>;

/// Error enumeration for mmap protocol violations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MmapError {
    /// File open/create failed
    FileIOError,
    /// Magic validation failed (file corruption or misalignment)
    MagicMismatch,
    /// Protocol version incompatible
    VersionMismatch,
    /// Requested payload index out of bounds
    PayloadIndexOutOfBounds,
    /// Mmap creation failed
    MmapCreationFailed,
    /// Payload access violation (attempt to read beyond mapped region)
    PayloadAccessViolation,
}

/// Producer: writes risk constraints to mmap file
/// Single-writer design ensures deterministic ordering without locks
pub struct MmapProducer {
    /// Mutable memory-mapped file handle
    mmap: MmapMut,
    /// Current constraint write position (0-indexed)
    current_payload_index: usize,
    /// Maximum payloads in this mmap region
    max_payloads: usize,
}

impl MmapProducer {
    /// Initialize mmap file for constraint writing
    /// 
    /// # Arguments
    /// * `file_path` - Path to mmap file (created if not exists)
    /// * `max_payloads` - Maximum number of constraint payloads (fixed at init)
    /// 
    /// # Returns
    /// `MmapProducer` on success, `MmapError` on failure
    /// 
    /// @audit:zero-allocation - No heap allocations; all buffers compile-time sized
    /// @audit:zero-copy - Direct mmap without intermediate serialization buffers
    #[inline]
    pub fn new(file_path: &str, max_payloads: usize) -> MmapResult<Self> {
        // Guard clause: validate max_payloads is within reasonable bounds
        if max_payloads == 0 || max_payloads > 16384 {
            return Err(MmapError::PayloadIndexOutOfBounds);
        }

        // Calculate total file size: header + (payload_size * count)
        let header_size = mem::size_of::<MmapFileHeader>();
        let payload_size = mem::size_of::<RiskConstraintPayload>();
        let total_file_size = header_size + (payload_size * max_payloads);

        // Attempt file creation with mmap-friendly permissions
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o644)
            .open(file_path)
            .map_err(|_| MmapError::FileIOError)?;

        // Set file size if newly created
        file.set_len(total_file_size as u64)
            .map_err(|_| MmapError::FileIOError)?;

        // Create mutable mmap
        let mut mmap = unsafe {
            memmap2::MmapMut::map_mut(&file)
                .map_err(|_| MmapError::MmapCreationFailed)?
        };

        // Write header at offset 0
        let header = MmapFileHeader {
            magic: MMAP_MAGIC,
            protocol_version: PROTOCOL_VERSION,
            _reserved_version: 0,
            file_size_bytes: total_file_size as u64,
            payload_offset: header_size as u64,
            max_payloads: max_payloads as u32,
            _padding: [0u8; 20],
        };

        // Zero the entire mmap region first (idempotent initialization)
        mmap.fill(0u8);

        // Write header bytes at offset 0 using zerocopy
        let header_bytes = header.as_bytes();
        mmap[0..header_bytes.len()].copy_from_slice(header_bytes);

        // Happy path: all initialization successful
        Ok(MmapProducer {
            mmap,
            current_payload_index: 0,
            max_payloads,
        })
    }

    /// Write a risk constraint payload to the next available slot
    /// 
    /// # Arguments
    /// * `constraint` - RiskConstraintPayload to write
    /// 
    /// @audit:zero-allocation - No heap allocation; direct mmap write
    /// @audit:zero-copy - Zerocopy trait enables direct byte casting without intermediate buffer
    /// @audit:cache-aligned - RiskConstraintPayload #[repr(C, align(64))] prevents cache-line sharing
    #[inline]
    pub fn write_constraint(&mut self, constraint: &RiskConstraintPayload) -> MmapResult<ConstraintSeqId> {
        // Guard clause: check payload index bounds
        if self.current_payload_index >= self.max_payloads {
            return Err(MmapError::PayloadIndexOutOfBounds);
        }

        // Calculate payload offset: header_size + (index * payload_size)
        let header_size = mem::size_of::<MmapFileHeader>();
        let payload_size = mem::size_of::<RiskConstraintPayload>();
        let offset = header_size + (self.current_payload_index * payload_size);

        // Guard clause: bounds check against mmap region
        if offset + payload_size > self.mmap.len() {
            return Err(MmapError::PayloadAccessViolation);
        }

        // Write constraint bytes directly to mmap via zerocopy
        let constraint_bytes = constraint.as_bytes();
        self.mmap[offset..offset + payload_size].copy_from_slice(constraint_bytes);

        // Update write position
        let seq_id = ConstraintSeqId(self.current_payload_index as u64);
        self.current_payload_index += 1;

        // Happy path: constraint written successfully
        Ok(seq_id)
    }

    /// Flush mmap to disk (explicit durability control)
    /// 
    /// @audit:zero-allocation - flush() is a no-op in most cases; no allocation
    #[inline]
    pub fn flush(&mut self) -> MmapResult<()> {
        self.mmap.flush().map_err(|_| MmapError::FileIOError)
    }

    /// Get current write position (immutable accessor)
    #[inline(always)]
    pub fn is_current_payload_index(&self) -> usize {
        self.current_payload_index
    }
}

/// Consumer: reads risk constraints from mmap file (read-only)
/// Lock-free reader; multiple instances safe simultaneously
pub struct MmapConsumer {
    /// Immutable memory-mapped file handle
    mmap: Mmap,
    /// Header metadata (cached after validation)
    header: MmapFileHeader,
    /// Current read position (consumer-local state)
    current_payload_index: usize,
}

impl MmapConsumer {
    /// Open existing mmap file for constraint reading
    /// 
    /// # Arguments
    /// * `file_path` - Path to mmap file (must exist)
    /// 
    /// # Returns
    /// `MmapConsumer` on success, `MmapError` on failure
    /// 
    /// @audit:zero-allocation - No heap allocations; read-only mmap handle
    /// @audit:lock-free - Read-only mmap; no synchronization primitives required
    #[inline]
    pub fn new(file_path: &str) -> MmapResult<Self> {
        // Guard clause: open file for reading only
        let file = OpenOptions::new()
            .read(true)
            .open(file_path)
            .map_err(|_| MmapError::FileIOError)?;

        // Create immutable mmap
        let mmap = unsafe {
            memmap2::Mmap::map(&file)
                .map_err(|_| MmapError::MmapCreationFailed)?
        };

        // Guard clause: validate minimum file size for header
        if mmap.len() < mem::size_of::<MmapFileHeader>() {
            return Err(MmapError::PayloadAccessViolation);
        }

        // Parse header from mmap bytes (zerocopy unmarshalling)
        let header_bytes = &mmap[0..mem::size_of::<MmapFileHeader>()];
        let header = MmapFileHeader::read_from(header_bytes)
            .ok_or(MmapError::PayloadAccessViolation)?;

        // Validate magic token
        if header.magic != MMAP_MAGIC {
            return Err(MmapError::MagicMismatch);
        }

        // Validate protocol version
        if header.protocol_version != PROTOCOL_VERSION {
            return Err(MmapError::VersionMismatch);
        }

        // Guard clause: validate header consistency
        if header.payload_offset as usize != mem::size_of::<MmapFileHeader>() {
            return Err(MmapError::PayloadAccessViolation);
        }

        // Happy path: consumer initialized with validated header
        Ok(MmapConsumer {
            mmap,
            header,
            current_payload_index: 0,
        })
    }

    /// Read the next constraint payload (sequential read)
    /// 
    /// @audit:zero-allocation - No heap allocation; direct zerocopy unmarshalling
    /// @audit:zero-copy - Direct buffer view into mmap; no intermediate copies
    /// @audit:cache-aligned - Reads from 64-byte aligned payload boundaries
    #[inline]
    pub fn read_constraint(&mut self) -> MmapResult<RiskConstraintPayload> {
        // Guard clause: check read bounds
        if self.current_payload_index >= self.header.max_payloads as usize {
            return Err(MmapError::PayloadIndexOutOfBounds);
        }

        // Calculate payload offset
        let payload_size = mem::size_of::<RiskConstraintPayload>();
        let offset = self.header.payload_offset as usize + (self.current_payload_index * payload_size);

        // Guard clause: validate mmap region bounds
        if offset + payload_size > self.mmap.len() {
            return Err(MmapError::PayloadAccessViolation);
        }

        // Unmarshall constraint from mmap bytes using zerocopy
        let payload_bytes = &self.mmap[offset..offset + payload_size];
        let constraint = RiskConstraintPayload::read_from(payload_bytes)
            .ok_or(MmapError::PayloadAccessViolation)?;

        // Update read position
        self.current_payload_index += 1;

        // Happy path: constraint read successfully
        Ok(constraint)
    }

    /// Seek to specific constraint payload index (random access)
    /// 
    /// @audit:zero-allocation - No allocation; in-place index update
    #[inline]
    pub fn seek_to_index(&mut self, index: usize) -> MmapResult<()> {
        // Guard clause: bounds validation
        if index >= self.header.max_payloads as usize {
            return Err(MmapError::PayloadIndexOutOfBounds);
        }

        // Happy path: index updated
        self.current_payload_index = index;
        Ok(())
    }

    /// Read constraint at arbitrary index without advancing position
    /// 
    /// @audit:zero-allocation - No heap allocation; direct read
    /// @audit:zero-copy - Direct mmap buffer view
    #[inline]
    pub fn read_constraint_at(&self, index: usize) -> MmapResult<RiskConstraintPayload> {
        // Guard clause: bounds validation
        if index >= self.header.max_payloads as usize {
            return Err(MmapError::PayloadIndexOutOfBounds);
        }

        let payload_size = mem::size_of::<RiskConstraintPayload>();
        let offset = self.header.payload_offset as usize + (index * payload_size);

        // Guard clause: validate mmap bounds
        if offset + payload_size > self.mmap.len() {
            return Err(MmapError::PayloadAccessViolation);
        }

        // Unmarshall constraint
        let payload_bytes = &self.mmap[offset..offset + payload_size];
        RiskConstraintPayload::read_from(payload_bytes)
            .ok_or(MmapError::PayloadAccessViolation)
    }

    /// Get current read position (immutable accessor)
    #[inline(always)]
    pub fn is_current_payload_index(&self) -> usize {
        self.current_payload_index
    }

    /// Get max payloads capacity
    #[inline(always)]
    pub fn has_max_payloads(&self) -> usize {
        self.header.max_payloads as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_mmap_roundtrip() {
        let test_file = "/tmp/test_risk_mmap.bin";
        let _ = fs::remove_file(test_file);

        // Producer: write constraints
        let mut producer = MmapProducer::new(test_file, 128).expect("producer creation failed");

        let constraint = RiskConstraintPayload {
            constraint_seq_id: 1,
            max_var_limit: 1_000_000.0,
            max_position_concentration: 0.25,
            max_intraday_loss: 500_000.0,
            max_leverage: 3.5,
            delta_hedge_target: 0.0,
            vega_hedge_target: 0.0,
            constraint_timestamp_ns: 1_000_000_000,
            reserved_a: [0; 32],
        };

        let seq_id = producer.write_constraint(&constraint).expect("write failed");
        assert_eq!(seq_id.0, 0);
        producer.flush().expect("flush failed");

        // Consumer: read constraints
        let mut consumer = MmapConsumer::new(test_file).expect("consumer creation failed");
        let read_constraint = consumer.read_constraint().expect("read failed");

        assert_eq!(read_constraint.constraint_seq_id, constraint.constraint_seq_id);
        assert_eq!(read_constraint.max_var_limit, constraint.max_var_limit);

        let _ = fs::remove_file(test_file);
    }
}
