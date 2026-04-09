-- Aeon PostgreSQL initialization
-- Creates tables for sink testing and CDC source testing

-- Sink target table
CREATE TABLE IF NOT EXISTS aeon_events (
    event_id UUID PRIMARY KEY,
    source_name TEXT NOT NULL,
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ DEFAULT NOW()
);

-- CDC source: change log table (outbox pattern)
CREATE TABLE IF NOT EXISTS change_log (
    id BIGSERIAL PRIMARY KEY,
    txn_id UUID NOT NULL,
    table_name TEXT NOT NULL,
    operation TEXT NOT NULL CHECK (operation IN ('INSERT', 'UPDATE', 'DELETE')),
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ DEFAULT NOW(),
    processed BOOLEAN DEFAULT FALSE
);

CREATE INDEX idx_change_log_unprocessed ON change_log (processed, id) WHERE NOT processed;

-- Logical replication publication for CDC
CREATE PUBLICATION aeon_cdc FOR TABLE change_log;

-- Insert seed data for testing
INSERT INTO change_log (txn_id, table_name, operation, payload)
VALUES
    (gen_random_uuid(), 'orders', 'INSERT', '{"order_id": 1, "amount": 99.99}'),
    (gen_random_uuid(), 'orders', 'INSERT', '{"order_id": 2, "amount": 149.50}'),
    (gen_random_uuid(), 'users', 'UPDATE', '{"user_id": 42, "name": "updated"}');
