// AssemblyScript JSON enrichment processor for Aeon.
//
// Implements the Aeon processor ABI:
// - alloc(size) -> ptr
// - dealloc(ptr, size)
// - process(ptr, len) -> ptr
//
// Uses raw memory operations only (no managed objects) for maximum
// compatibility with the Aeon host runtime.

// ── Host imports ────────────────────────────────────────────────────────

@external("env", "log_info")
declare function host_log_info(ptr: i32, len: i32): void;

@external("env", "clock_now_ms")
declare function host_clock_now_ms(): i64;

// ── Bump allocator ──────────────────────────────────────────────────────

// Use memory page 2+ for our bump allocator (pages 0-1 for data/stack)
let heapOffset: i32 = 131072; // 2 * 64KB

export function alloc(size: i32): i32 {
  const ptr = heapOffset;
  heapOffset += (size + 7) & ~7;
  return ptr;
}

export function dealloc(ptr: i32, size: i32): void {
  // No-op bump allocator
}

// ── Wire format helpers ─────────────────────────────────────────────────

function readU32LE(ptr: i32): u32 {
  return load<u8>(ptr)
    | (load<u8>(ptr + 1) << 8)
    | (load<u8>(ptr + 2) << 16)
    | (load<u8>(ptr + 3) << 24);
}

function writeU32LE(ptr: i32, val: u32): void {
  store<u8>(ptr, val & 0xFF);
  store<u8>(ptr + 1, (val >> 8) & 0xFF);
  store<u8>(ptr + 2, (val >> 16) & 0xFF);
  store<u8>(ptr + 3, (val >> 24) & 0xFF);
}

function writeBytes(dstPtr: i32, srcPtr: i32, len: i32): void {
  memory.copy(dstPtr, srcPtr, len);
}

// Store constant strings in data segment via memory.data
// "enriched" at offset 1024
// {"original": at offset 1040
// ,"extracted_user_id":" at offset 1056
// "} at offset 1080

// ── JSON field extraction ───────────────────────────────────────────────

function findUserIdValue(payloadPtr: i32, payloadLen: i32): i64 {
  // Search for "user_id":" pattern
  for (let i: i32 = 0; i < payloadLen - 10; i++) {
    if (
      load<u8>(payloadPtr + i)     == 0x22 && // "
      load<u8>(payloadPtr + i + 1) == 0x75 && // u
      load<u8>(payloadPtr + i + 2) == 0x73 && // s
      load<u8>(payloadPtr + i + 3) == 0x65 && // e
      load<u8>(payloadPtr + i + 4) == 0x72 && // r
      load<u8>(payloadPtr + i + 5) == 0x5F && // _
      load<u8>(payloadPtr + i + 6) == 0x69 && // i
      load<u8>(payloadPtr + i + 7) == 0x64 && // d
      load<u8>(payloadPtr + i + 8) == 0x22 && // "
      load<u8>(payloadPtr + i + 9) == 0x3A    // :
    ) {
      let valStart = payloadPtr + i + 10;
      // Skip whitespace
      while (valStart < payloadPtr + payloadLen && load<u8>(valStart) == 0x20) {
        valStart++;
      }
      // Expect opening quote
      if (load<u8>(valStart) == 0x22) {
        valStart++;
        let valEnd = valStart;
        while (valEnd < payloadPtr + payloadLen && load<u8>(valEnd) != 0x22) {
          valEnd++;
        }
        // Return (valStart, valLen) packed as i64
        const valLen = valEnd - valStart;
        return (<i64>valStart << 32) | <i64>valLen;
      }
    }
  }
  return -1;
}

// ── Process function ────────────────────────────────────────────────────

export function process(ptr: i32, len: i32): i32 {
  // Reset bump allocator
  heapOffset = 131072;

  // Parse serialized event — skip to payload
  let pos = ptr + 24; // skip UUID (16) + timestamp (8)

  // Skip source string
  const srcLen = readU32LE(pos) as i32;
  pos += 4 + srcLen;

  // Skip partition (2 bytes)
  pos += 2;

  // Skip metadata
  const metaCount = readU32LE(pos) as i32;
  pos += 4;
  for (let m: i32 = 0; m < metaCount; m++) {
    const kl = readU32LE(pos) as i32;
    pos += 4 + kl;
    const vl = readU32LE(pos) as i32;
    pos += 4 + vl;
  }

  // Read payload
  const payloadLen = readU32LE(pos) as i32;
  pos += 4;
  const payloadPtr = pos;

  // Extract user_id
  const found = findUserIdValue(payloadPtr, payloadLen);

  // Destination = "enriched" (8 bytes)
  const destLen: i32 = 8;

  let outputPayloadLen: i32;
  let outputPayloadPtr: i32;

  if (found >= 0) {
    const valPtr: i32 = <i32>(found >> 32);
    const valLen: i32 = <i32>(found & 0xFFFFFFFF);

    // {"original":
    const prefixLen: i32 = 12;
    // ,"extracted_user_id":"
    const middleLen: i32 = 22;
    // "}
    const suffixLen: i32 = 2;

    outputPayloadLen = prefixLen + payloadLen + middleLen + valLen + suffixLen;
    outputPayloadPtr = alloc(outputPayloadLen);

    let wp = outputPayloadPtr;

    // Write {"original":
    store<u8>(wp, 0x7B); store<u8>(wp+1, 0x22); store<u8>(wp+2, 0x6F); store<u8>(wp+3, 0x72);
    store<u8>(wp+4, 0x69); store<u8>(wp+5, 0x67); store<u8>(wp+6, 0x69); store<u8>(wp+7, 0x6E);
    store<u8>(wp+8, 0x61); store<u8>(wp+9, 0x6C); store<u8>(wp+10, 0x22); store<u8>(wp+11, 0x3A);
    wp += prefixLen;

    // Copy original payload
    memory.copy(wp, payloadPtr, payloadLen);
    wp += payloadLen;

    // Write ,"extracted_user_id":"
    store<u8>(wp, 0x2C); store<u8>(wp+1, 0x22); store<u8>(wp+2, 0x65); store<u8>(wp+3, 0x78);
    store<u8>(wp+4, 0x74); store<u8>(wp+5, 0x72); store<u8>(wp+6, 0x61); store<u8>(wp+7, 0x63);
    store<u8>(wp+8, 0x74); store<u8>(wp+9, 0x65); store<u8>(wp+10, 0x64); store<u8>(wp+11, 0x5F);
    store<u8>(wp+12, 0x75); store<u8>(wp+13, 0x73); store<u8>(wp+14, 0x65); store<u8>(wp+15, 0x72);
    store<u8>(wp+16, 0x5F); store<u8>(wp+17, 0x69); store<u8>(wp+18, 0x64); store<u8>(wp+19, 0x22);
    store<u8>(wp+20, 0x3A); store<u8>(wp+21, 0x22);
    wp += middleLen;

    // Copy extracted value
    memory.copy(wp, valPtr, valLen);
    wp += valLen;

    // Write "}
    store<u8>(wp, 0x22); store<u8>(wp+1, 0x7D);
  } else {
    outputPayloadLen = payloadLen;
    outputPayloadPtr = alloc(payloadLen);
    memory.copy(outputPayloadPtr, payloadPtr, payloadLen);
  }

  // Serialize output list
  const contentLen: i32 = 4 + 4 + destLen + 1 + 4 + outputPayloadLen + 4;
  const resultPtr = alloc(4 + contentLen);

  let wp = resultPtr;

  // Length prefix
  writeU32LE(wp, contentLen as u32); wp += 4;
  // Output count = 1
  writeU32LE(wp, 1); wp += 4;
  // Destination length = 8
  writeU32LE(wp, destLen as u32); wp += 4;
  // "enriched"
  store<u8>(wp, 0x65); store<u8>(wp+1, 0x6E); store<u8>(wp+2, 0x72); store<u8>(wp+3, 0x69);
  store<u8>(wp+4, 0x63); store<u8>(wp+5, 0x68); store<u8>(wp+6, 0x65); store<u8>(wp+7, 0x64);
  wp += destLen;
  // No key
  store<u8>(wp, 0); wp += 1;
  // Payload length
  writeU32LE(wp, outputPayloadLen as u32); wp += 4;
  // Payload bytes
  memory.copy(wp, outputPayloadPtr, outputPayloadLen);
  wp += outputPayloadLen;
  // Header count = 0
  writeU32LE(wp, 0);

  return resultPtr;
}
