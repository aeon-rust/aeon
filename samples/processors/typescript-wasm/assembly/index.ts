/**
 * Sample Aeon Wasm processor in TypeScript (AssemblyScript).
 *
 * Extracts "user_id" from JSON payloads and emits enriched output.
 * Same logic as the Rust Wasm sample, demonstrating the TypeScript SDK.
 *
 * Build: npm run asbuild
 * Validate: aeon validate build/release.wasm
 */

import {
  Event,
  Output,
  registerProcessor,
  logInfo,
  counterInc,
} from "@aeon/wasm-sdk/assembly/index";

registerProcessor((event: Event): Output[] => {
  const payload = event.payloadString();

  // Try to extract user_id from JSON
  const userId = extractJsonField(payload, "user_id");

  counterInc("events_processed", 1);

  if (userId !== null) {
    logInfo("extracted user_id: " + userId!);
    const enriched =
      '{"original":' + payload + ',"extracted_user_id":"' + userId! + '"}';
    return [Output.fromString("enriched", enriched)];
  }

  // No user_id found — passthrough
  return [new Output("enriched", event.payload)];
});

/**
 * Simple JSON field extractor — finds "field":"value" and returns value.
 * Works for simple string values only.
 */
function extractJsonField(json: string, field: string): string | null {
  const searchKey = '"' + field + '":';
  const idx = json.indexOf(searchKey);
  if (idx < 0) return null;

  let valStart = idx + searchKey.length;

  // Skip whitespace
  while (
    valStart < json.length &&
    (json.charCodeAt(valStart) == 32 || json.charCodeAt(valStart) == 9)
  ) {
    valStart++;
  }

  // Expect opening quote
  if (valStart >= json.length || json.charCodeAt(valStart) != 34) return null;
  valStart++;

  // Find closing quote
  let valEnd = valStart;
  while (valEnd < json.length && json.charCodeAt(valEnd) != 34) {
    valEnd++;
  }

  return json.substring(valStart, valEnd);
}

// Re-export Wasm ABI functions so they're visible as module exports
export { alloc, dealloc, process } from "@aeon/wasm-sdk/assembly/index";
