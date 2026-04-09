//! Tier G E2E Tests: CDC Database Sources
//!
//! Tests from `docs/E2E-TEST-PLAN.md` Tier G (P3, heavy infra — databases required).
//! Change Data Capture sources feeding into various sinks.
//!
//! Each test verifies the 5 E2E criteria:
//!   1. Event delivery: source count == sink count (zero loss)
//!   2. Payload integrity: CDC payload contains expected row data
//!   3. Metadata propagation: schema/table info preserved
//!   4. Ordering: within a partition, events arrive in order
//!   5. Graceful shutdown: clean termination, CDC slot cleaned up

// ===========================================================================
// G1: PostgreSQL CDC -> Kafka (Rust Native T1)
// ===========================================================================

#[tokio::test]
#[ignore = "requires PostgreSQL 16 + Redpanda (Docker) — heavy infra"]
async fn g1_postgres_cdc_kafka_rust_native_t1() {
    // Setup: CREATE TABLE test_events; INSERT rows; logical replication slot.
    // Pipeline: PostgresCdcSource -> PassthroughProcessor -> KafkaSink.
    // Verify: all INSERT/UPDATE/DELETE events land in Kafka topic.
    todo!("Implement with PostgresCdcSource + Rust Native T1 + KafkaSink");
}

// ===========================================================================
// G2: MySQL CDC -> Memory (Python T4)
// ===========================================================================

#[tokio::test]
#[ignore = "requires MySQL 8 (Docker) + Python SDK + engine WebSocket host"]
async fn g2_mysql_cdc_memory_python_t4() {
    // Setup: CREATE TABLE; INSERT rows; binlog CDC.
    // Pipeline: MysqlCdcSource -> Python T4 -> MemorySink.
    todo!("Implement with MysqlCdcSource + Python T4 + MemorySink");
}

// ===========================================================================
// G3: MongoDB CDC -> Memory (Node.js T4)
// ===========================================================================

#[tokio::test]
#[ignore = "requires MongoDB 7 (Docker) + Node.js SDK + engine WebSocket host"]
async fn g3_mongodb_cdc_memory_nodejs_t4() {
    // Setup: insert documents; change stream CDC.
    // Pipeline: MongoDbCdcSource -> Node.js T4 -> MemorySink.
    todo!("Implement with MongoDbCdcSource + Node.js T4 + MemorySink");
}
