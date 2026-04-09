/**
 * Aeon Wasm Processor SDK for AssemblyScript / TypeScript.
 *
 * This is the main entry point. Users import types and SDK functions from here,
 * then implement their processor logic and call `registerProcessor()`.
 *
 * ## Quick Start
 *
 * ```typescript
 * import { Event, Output, registerProcessor } from "@aeon/wasm-sdk";
 *
 * registerProcessor((event: Event): Output[] => {
 *   return [new Output("output-topic", event.payload)];
 * });
 * ```
 *
 * The SDK generates the required Wasm exports (alloc, dealloc, process)
 * and handles all wire format serialization automatically.
 */

// Re-export types
export { Event, Output, Header } from "./types";

// Re-export SDK functions
export {
  stateGet,
  statePut,
  stateDelete,
  logInfo,
  logWarn,
  logError,
  clockNowMs,
  counterInc,
  gaugeSet,
} from "./sdk";

import { Event, Output } from "./types";
import { deserializeEvent, serializeOutputs } from "./wire";

/** User's processor function type. */
type ProcessorFn = (event: Event) => Output[];

/** Global reference to the user's processor function. */
let _processorFn: ProcessorFn | null = null;

/**
 * Register a processor function. Must be called once at module initialization.
 *
 * ```typescript
 * registerProcessor((event: Event): Output[] => {
 *   return [Output.fromString("output", event.payloadString())];
 * });
 * ```
 */
export function registerProcessor(fn: ProcessorFn): void {
  _processorFn = fn;
}

// ── Wasm ABI exports ──────────────────────────────────────────────────

/** Allocate memory in guest (called by host to pass data in). */
export function alloc(size: i32): i32 {
  return heap.alloc(size) as i32;
}

/** Free memory in guest (called by host after reading results). */
export function dealloc(ptr: i32, size: i32): void {
  heap.free(ptr as usize);
}

/**
 * Process a serialized event and return a pointer to serialized outputs.
 *
 * Called by the Aeon host. The host writes a serialized event at `ptr`,
 * this function deserializes it, calls the user's processor, serializes
 * the outputs, and returns a pointer to the result.
 *
 * Return format: [4B: result_len][result_bytes...]
 */
export function process(ptr: i32, len: i32): i32 {
  if (_processorFn === null) {
    // No processor registered — return empty output list
    return writeEmptyResult();
  }

  // Deserialize event from wire format
  const event = deserializeEvent(ptr as usize, len as usize);

  // Call user's processor
  const outputs = _processorFn!(event);

  // Serialize outputs to wire format (includes length prefix)
  return serializeOutputs(outputs) as i32;
}

function writeEmptyResult(): i32 {
  const ptr = heap.alloc(8);
  store<u32>(ptr, 4); // length prefix = 4
  store<u32>(ptr + 4, 0); // count = 0
  return ptr as i32;
}
