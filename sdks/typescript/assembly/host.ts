/**
 * Raw host import declarations.
 *
 * These functions are provided by the Aeon Wasm runtime via the "env" module.
 * Users should prefer the safe wrappers in state.ts, log.ts, metrics.ts, clock.ts.
 */

// State
// @ts-ignore: decorator valid in AssemblyScript
@external("env", "state_get")
export declare function state_get(
  key_ptr: i32,
  key_len: i32,
  out_ptr: i32,
  out_max_len: i32,
): i32;

// @ts-ignore: decorator valid in AssemblyScript
@external("env", "state_put")
export declare function state_put(
  key_ptr: i32,
  key_len: i32,
  val_ptr: i32,
  val_len: i32,
): void;

// @ts-ignore: decorator valid in AssemblyScript
@external("env", "state_delete")
export declare function state_delete(key_ptr: i32, key_len: i32): void;

// Logging
// @ts-ignore: decorator valid in AssemblyScript
@external("env", "log_info")
export declare function log_info(ptr: i32, len: i32): void;

// @ts-ignore: decorator valid in AssemblyScript
@external("env", "log_warn")
export declare function log_warn(ptr: i32, len: i32): void;

// @ts-ignore: decorator valid in AssemblyScript
@external("env", "log_error")
export declare function log_error(ptr: i32, len: i32): void;

// Clock
// @ts-ignore: decorator valid in AssemblyScript
@external("env", "clock_now_ms")
export declare function clock_now_ms(): i64;

// Metrics
// @ts-ignore: decorator valid in AssemblyScript
@external("env", "metrics_counter_inc")
export declare function metrics_counter_inc(
  name_ptr: i32,
  name_len: i32,
  delta: i64,
): void;

// @ts-ignore: decorator valid in AssemblyScript
@external("env", "metrics_gauge_set")
export declare function metrics_gauge_set(
  name_ptr: i32,
  name_len: i32,
  value: f64,
): void;
