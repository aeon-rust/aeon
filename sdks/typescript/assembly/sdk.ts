/**
 * Safe wrappers for Aeon host imports.
 *
 * These functions wrap the raw FFI calls from host.ts with ergonomic
 * TypeScript-friendly APIs.
 */

import {
  state_get,
  state_put,
  state_delete,
  log_info,
  log_warn,
  log_error,
  clock_now_ms,
  metrics_counter_inc,
  metrics_gauge_set,
} from "./host";

// ── State ──────────────────────────────────────────────────────────────

const STATE_BUF_SIZE: i32 = 65536;

/** Get a value by key from the host state store. Returns null if not found. */
export function stateGet(key: Uint8Array): Uint8Array | null {
  const outBuf = heap.alloc(STATE_BUF_SIZE);
  const result = state_get(
    changetype<i32>(key.buffer),
    key.length,
    outBuf as i32,
    STATE_BUF_SIZE,
  );
  if (result < 0) {
    heap.free(outBuf);
    return null;
  }
  const out = new Uint8Array(result);
  memory.copy(changetype<usize>(out.buffer), outBuf, result as usize);
  heap.free(outBuf);
  return out;
}

/** Put a key-value pair into the host state store. */
export function statePut(key: Uint8Array, value: Uint8Array): void {
  state_put(
    changetype<i32>(key.buffer),
    key.length,
    changetype<i32>(value.buffer),
    value.length,
  );
}

/** Delete a key from the host state store. */
export function stateDelete(key: Uint8Array): void {
  state_delete(changetype<i32>(key.buffer), key.length);
}

// ── Logging ───────────────────────────────────────────���────────────────

/** Log at info level. */
export function logInfo(msg: string): void {
  const buf = String.UTF8.encode(msg);
  log_info(changetype<i32>(buf), buf.byteLength as i32);
}

/** Log at warn level. */
export function logWarn(msg: string): void {
  const buf = String.UTF8.encode(msg);
  log_warn(changetype<i32>(buf), buf.byteLength as i32);
}

/** Log at error level. */
export function logError(msg: string): void {
  const buf = String.UTF8.encode(msg);
  log_error(changetype<i32>(buf), buf.byteLength as i32);
}

// ── Clock ──────────────────────────────────────────────────────────────

/** Get current time in milliseconds since Unix epoch. */
export function clockNowMs(): i64 {
  return clock_now_ms();
}

// ── Metrics ────────────────────────────────────────────────────────────

/** Increment a counter metric. */
export function counterInc(name: string, delta: u64): void {
  const buf = String.UTF8.encode(name);
  metrics_counter_inc(changetype<i32>(buf), buf.byteLength as i32, delta as i64);
}

/** Set a gauge metric value. */
export function gaugeSet(name: string, value: f64): void {
  const buf = String.UTF8.encode(name);
  metrics_gauge_set(changetype<i32>(buf), buf.byteLength as i32, value);
}
