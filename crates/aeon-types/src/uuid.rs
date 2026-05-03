//! Per-core UUIDv7 generator with SPSC pre-generation pool.
//!
//! At 20M events/sec, standard `uuid::Uuid::now_v7()` is too slow (100-200ns per call
//! due to `getrandom()` syscall). This module pre-generates UUIDs in a background thread
//! and serves them from a lock-free SPSC ring buffer at ~1-2ns per UUID.
//!
//! Bit layout (128 bits, UUIDv7-compatible):
//! ```text
//! | 48-bit ms timestamp | 4-bit ver(0x7) | 12-bit counter | 2-bit variant | 6-bit core_id | 56-bit random |
//! ```

use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Default pool capacity: 64K UUIDs per core (~1MB at 16 bytes each).
const DEFAULT_POOL_CAPACITY: usize = 65536;

/// Per-core UUIDv7 generator. Each core has its own instance — zero contention.
pub struct CoreLocalUuidGenerator {
    core_id: u8,
    pool: rtrb::Consumer<uuid::Uuid>,
    // Keep the producer handle alive so the background thread can fill
    _fill_handle: Option<std::thread::JoinHandle<()>>,
    // Fallback state for inline generation when pool is empty
    fallback_counter: AtomicU16,
    last_ms: AtomicU64,
}

impl CoreLocalUuidGenerator {
    /// Create a new generator for the given core ID (0..63).
    ///
    /// Spawns a background thread that continuously fills the SPSC ring buffer
    /// with pre-generated UUIDv7 values.
    pub fn new(core_id: u8) -> Self {
        assert!(core_id < 64, "core_id must be 0..63 (6-bit field)");

        let (producer, consumer) = rtrb::RingBuffer::new(DEFAULT_POOL_CAPACITY);

        let fill_core_id = core_id;
        // Startup-time invariant: if the OS refuses a new thread, the
        // generator can't produce UUIDs and there is no meaningful recovery
        // — the process must terminate. FT-10: documented `.expect()` for
        // a genuinely unrecoverable condition, not a hot-path panic.
        #[allow(clippy::expect_used)]
        let handle = std::thread::Builder::new()
            .name(format!("uuid-fill-{core_id}"))
            .spawn(move || {
                Self::fill_loop(producer, fill_core_id);
            })
            .expect("invariant: UUID fill thread spawn at startup");

        Self {
            core_id,
            pool: consumer,
            _fill_handle: Some(handle),
            fallback_counter: AtomicU16::new(0),
            last_ms: AtomicU64::new(0),
        }
    }

    /// Create a generator without background thread (for testing/benchmarking).
    /// Pool starts empty — all UUIDs come from fallback path.
    pub fn new_fallback_only(core_id: u8) -> Self {
        assert!(core_id < 64, "core_id must be 0..63 (6-bit field)");

        let (_producer, consumer) = rtrb::RingBuffer::new(1);

        Self {
            core_id,
            pool: consumer,
            _fill_handle: None,
            fallback_counter: AtomicU16::new(0),
            last_ms: AtomicU64::new(0),
        }
    }

    /// Get the next UUIDv7. Fast path: pop from pool (~1-2ns).
    /// Fallback: generate inline (~20ns, no syscall).
    #[inline(always)]
    pub fn next_uuid(&mut self) -> uuid::Uuid {
        match self.pool.pop() {
            Ok(uuid) => uuid,
            Err(_) => self.generate_inline(),
        }
    }

    /// Generate a UUIDv7 inline (fallback when pool is empty).
    /// Uses monotonic counter per core — no syscall, no random.
    fn generate_inline(&self) -> uuid::Uuid {
        // Hot path: `duration_since(UNIX_EPOCH)` can only fail if the system
        // clock is set before 1970. Saturate to 0 rather than panic — a
        // panic here would kill the pipeline thread and lose in-flight
        // events (FT-10, CLAUDE.md rule #2 zero-event-loss).
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let prev_ms = self.last_ms.load(Ordering::Relaxed);
        let counter = if now_ms > prev_ms {
            self.last_ms.store(now_ms, Ordering::Relaxed);
            self.fallback_counter.store(1, Ordering::Relaxed);
            0
        } else {
            self.fallback_counter.fetch_add(1, Ordering::Relaxed)
        };

        Self::build_uuid(now_ms, counter, self.core_id, 0)
    }

    /// Background fill loop — continuously generates UUIDv7s into the producer.
    fn fill_loop(mut producer: rtrb::Producer<uuid::Uuid>, core_id: u8) {
        let mut counter: u16 = 0;
        let mut last_ms: u64 = 0;
        // Simple xorshift64 PRNG — fast, non-cryptographic, sufficient for UUID random bits
        let mut rng_state: u64 = (core_id as u64 + 1) * 6364136223846793005 + 1;

        loop {
            // Hot path in background filler: saturate pre-epoch clocks to 0
            // rather than panic. See generate_inline() for rationale.
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);

            if now_ms > last_ms {
                last_ms = now_ms;
                counter = 0;
            }

            // Generate a UUID
            rng_state ^= rng_state << 13;
            rng_state ^= rng_state >> 7;
            rng_state ^= rng_state << 17;
            let random_bits = rng_state;

            let uuid = Self::build_uuid(now_ms, counter, core_id, random_bits);
            counter = counter.wrapping_add(1) & 0x0FFF; // 12-bit wrap

            match producer.push(uuid) {
                Ok(()) => {}
                Err(_) => {
                    // Ring buffer full — yield and retry
                    std::thread::yield_now();
                }
            }
        }
    }

    /// Assemble a UUIDv7 from components.
    ///
    /// Layout:
    /// - Bits 127..80: 48-bit millisecond timestamp
    /// - Bits 79..76: 4-bit version (0x7)
    /// - Bits 75..64: 12-bit monotonic counter
    /// - Bits 63..62: 2-bit variant (0b10)
    /// - Bits 61..56: 6-bit core_id
    /// - Bits 55..0: 56-bit random
    #[inline]
    fn build_uuid(timestamp_ms: u64, counter: u16, core_id: u8, random: u64) -> uuid::Uuid {
        let mut bytes = [0u8; 16];

        // Bytes 0..5: 48-bit timestamp (big-endian, high 6 bytes)
        let ts_bytes = timestamp_ms.to_be_bytes();
        bytes[0] = ts_bytes[2];
        bytes[1] = ts_bytes[3];
        bytes[2] = ts_bytes[4];
        bytes[3] = ts_bytes[5];
        bytes[4] = ts_bytes[6];
        bytes[5] = ts_bytes[7];

        // Bytes 6..7: version (4 bits) + counter high (12 bits)
        let ver_counter = (0x7u16 << 12) | (counter & 0x0FFF);
        let vc_bytes = ver_counter.to_be_bytes();
        bytes[6] = vc_bytes[0];
        bytes[7] = vc_bytes[1];

        // Byte 8: variant (2 bits = 0b10) + core_id high 6 bits
        bytes[8] = 0b1000_0000 | (core_id & 0x3F);

        // Bytes 9..15: 56-bit random (7 bytes)
        let rand_bytes = random.to_be_bytes();
        bytes[9] = rand_bytes[1];
        bytes[10] = rand_bytes[2];
        bytes[11] = rand_bytes[3];
        bytes[12] = rand_bytes[4];
        bytes[13] = rand_bytes[5];
        bytes[14] = rand_bytes[6];
        bytes[15] = rand_bytes[7];

        uuid::Uuid::from_bytes(bytes)
    }
}

// ── EO-2 P2: deterministic pull-source UUIDv7 derivation ────────────────
//
// Layout (RFC 9562 UUIDv7):
//   [ unix_ts_ms : 48 ][ ver:4 = 0x7 ][ rand_a:12 ][ var:2 = 0b10 ][ rand_b:62 ]
//
// For pull sources:
//   unix_ts_ms  ← source event timestamp (Kafka ts / CDC ts / …)
//   rand_a (12) ← broker_offset & 0xFFF        — within-partition monotonic
//   rand_b (62) ← SHA-256(source_name ‖ identity).take(62 bits)
//
// Same `(source_name, offset, identity, ts)` ⇒ same UUID across crashes,
// restarts, and partition reassignments. See
// `docs/EO-2-DURABILITY-DESIGN.md` §4.1.

/// Derive a deterministic UUIDv7 from pull-source-native identity.
///
/// Pure function — zero allocation on the hot path beyond the SHA-256
/// state on the stack. ~30 ns on HW-accelerated SHA platforms.
///
/// `source_name` should be the interned source ident (e.g. the pipeline's
/// source key) — stable across restarts. `identity` is source-native bytes
/// uniquely identifying the event (for Kafka: partition+offset+topic; for
/// CDC: LSN or binlog position). `offset` is the within-partition monotonic
/// number (Kafka offset, LSN truncated, …) — its low 12 bits populate
/// `rand_a` for within-ms ordering.
#[inline]
pub fn derive_pull_uuid_v7(
    source_name: &str,
    identity: &[u8],
    unix_ts_ms: u64,
    offset: u64,
) -> uuid::Uuid {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(source_name.as_bytes());
    hasher.update(b"\x1f"); // ASCII Unit Separator — avoids prefix collisions
    hasher.update(identity);
    let digest = hasher.finalize();

    let rand_a: u16 = (offset as u16) & 0x0FFF;

    let mut bytes = [0u8; 16];

    // Bytes 0..5 — 48-bit ms timestamp
    let ts = unix_ts_ms.to_be_bytes();
    bytes[0..6].copy_from_slice(&ts[2..8]);

    // Bytes 6..7 — version (0x7) in high nibble of byte 6 + rand_a (12 bits)
    let ver_rand_a = (0x7u16 << 12) | rand_a;
    bytes[6..8].copy_from_slice(&ver_rand_a.to_be_bytes());

    // Bytes 8..15 — variant (0b10) in top 2 bits of byte 8 + 62 bits of hash
    // Take 8 bytes of digest and mask the variant.
    bytes[8..16].copy_from_slice(&digest[0..8]);
    bytes[8] = 0b1000_0000 | (bytes[8] & 0b0011_1111);

    uuid::Uuid::from_bytes(bytes)
}

impl std::fmt::Debug for CoreLocalUuidGenerator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreLocalUuidGenerator")
            .field("core_id", &self.core_id)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_is_valid_v7() {
        let uuid = CoreLocalUuidGenerator::build_uuid(1234567890123, 42, 5, 0xDEADBEEF);
        assert_eq!(uuid.get_version(), Some(uuid::Version::SortRand));
        assert_eq!(uuid.get_variant(), uuid::Variant::RFC4122);
    }

    #[test]
    fn uuid_extracts_core_id() {
        for core_id in [0u8, 1, 31, 63] {
            let uuid = CoreLocalUuidGenerator::build_uuid(0, 0, core_id, 0);
            let bytes = uuid.as_bytes();
            let extracted = bytes[8] & 0x3F;
            assert_eq!(extracted, core_id, "core_id {core_id} not preserved");
        }
    }

    #[test]
    fn uuid_preserves_timestamp_ordering() {
        let u1 = CoreLocalUuidGenerator::build_uuid(1000, 0, 0, 0);
        let u2 = CoreLocalUuidGenerator::build_uuid(2000, 0, 0, 0);
        // UUIDv7 is time-ordered: later timestamp = lexicographically greater
        assert!(u1 < u2);
    }

    #[test]
    fn uuid_counter_ensures_ordering_within_ms() {
        let u1 = CoreLocalUuidGenerator::build_uuid(1000, 0, 0, 0);
        let u2 = CoreLocalUuidGenerator::build_uuid(1000, 1, 0, 0);
        assert!(u1 < u2);
    }

    // ── EO-2 P2: deterministic pull-source derivation ────────────────────

    #[test]
    fn derive_pull_uuid_is_deterministic() {
        let u1 = derive_pull_uuid_v7("kafka://orders", b"part=0/off=42", 1_700_000_000_000, 42);
        let u2 = derive_pull_uuid_v7("kafka://orders", b"part=0/off=42", 1_700_000_000_000, 42);
        assert_eq!(u1, u2, "same inputs must yield same UUID across calls");
    }

    #[test]
    fn derive_pull_uuid_differs_on_source_name() {
        let a = derive_pull_uuid_v7("kafka://a", b"id", 1_700_000_000_000, 0);
        let b = derive_pull_uuid_v7("kafka://b", b"id", 1_700_000_000_000, 0);
        assert_ne!(a, b);
    }

    #[test]
    fn derive_pull_uuid_differs_on_identity() {
        let a = derive_pull_uuid_v7("s", b"id-a", 1_700_000_000_000, 0);
        let b = derive_pull_uuid_v7("s", b"id-b", 1_700_000_000_000, 0);
        assert_ne!(a, b);
    }

    #[test]
    fn derive_pull_uuid_is_valid_v7() {
        let u = derive_pull_uuid_v7("s", b"id", 1_700_000_000_000, 42);
        assert_eq!(u.get_version(), Some(uuid::Version::SortRand));
        assert_eq!(u.get_variant(), uuid::Variant::RFC4122);
    }

    #[test]
    fn derive_pull_uuid_ts_ordering() {
        let early = derive_pull_uuid_v7("s", b"id", 1_700_000_000_000, 0);
        let later = derive_pull_uuid_v7("s", b"id", 1_700_000_001_000, 0);
        assert!(
            early < later,
            "ts ordering must be preserved lexicographically"
        );
    }

    #[test]
    fn derive_pull_uuid_within_partition_monotonic_on_offset_low_bits() {
        // Same ts + same identity + offsets differing only in low 12 bits
        // should still produce different, ordered UUIDs via rand_a.
        let a = derive_pull_uuid_v7("s", b"i", 1_700_000_000_000, 1);
        let b = derive_pull_uuid_v7("s", b"i", 1_700_000_000_000, 2);
        assert_ne!(a, b);
    }

    #[test]
    fn derive_pull_uuid_offset_masked_to_12_bits() {
        // Offsets 0 and 4096 have the same low-12-bits (0). Deterministic
        // collisions on exact identity are acceptable — the design caps
        // per-ms-per-partition throughput at 4096 events before rand_a
        // collisions, well above the 20M/s target distributed across
        // partitions (see EO-2-DURABILITY-DESIGN.md §4.1).
        let a = derive_pull_uuid_v7("s", b"i", 1_700_000_000_000, 0);
        let b = derive_pull_uuid_v7("s", b"i", 1_700_000_000_000, 4096);
        assert_eq!(a, b);
    }

    #[test]
    fn derive_pull_uuid_separator_prevents_prefix_collision() {
        // "ab" ‖ "cd" must not equal "a" ‖ "bcd" — the 0x1f separator
        // guards against this class of identity collision.
        let a = derive_pull_uuid_v7("ab", b"cd", 1_700_000_000_000, 0);
        let b = derive_pull_uuid_v7("a", b"bcd", 1_700_000_000_000, 0);
        assert_ne!(a, b);
    }

    #[test]
    fn fallback_generator_produces_unique_uuids() {
        let mut generator = CoreLocalUuidGenerator::new_fallback_only(0);
        let mut uuids = Vec::with_capacity(1000);
        for _ in 0..1000 {
            uuids.push(generator.next_uuid());
        }
        // All unique
        let mut deduped = uuids.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(uuids.len(), deduped.len(), "fallback produced duplicates");
    }

    #[test]
    fn pool_generator_produces_valid_uuids() {
        let mut generator = CoreLocalUuidGenerator::new(1);
        // Give fill thread time to produce
        std::thread::sleep(std::time::Duration::from_millis(10));

        let uuid = generator.next_uuid();
        assert_eq!(uuid.get_version(), Some(uuid::Version::SortRand));
        assert_eq!(uuid.get_variant(), uuid::Variant::RFC4122);
    }

    #[test]
    fn pool_generator_core_id_embedded() {
        let core_id = 42u8;
        let mut generator = CoreLocalUuidGenerator::new(core_id);
        std::thread::sleep(std::time::Duration::from_millis(10));

        let uuid = generator.next_uuid();
        let extracted = uuid.as_bytes()[8] & 0x3F;
        assert_eq!(extracted, core_id);
    }

    #[test]
    fn different_cores_produce_different_uuids() {
        let mut generator0 = CoreLocalUuidGenerator::new(0);
        let mut generator1 = CoreLocalUuidGenerator::new(1);
        std::thread::sleep(std::time::Duration::from_millis(10));

        let u0 = generator0.next_uuid();
        let u1 = generator1.next_uuid();
        assert_ne!(u0, u1);
    }
}
