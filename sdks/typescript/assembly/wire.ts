/**
 * Binary wire format serialization/deserialization.
 *
 * Matches the exact format used by the Aeon engine host:
 *
 * Event (host → guest):
 *   [16B: UUID][8B: timestamp i64 LE][4B: source_len][source]
 *   [2B: partition u16 LE][4B: meta_count]
 *     for each: [4B: key_len][key][4B: val_len][val]
 *   [4B: payload_len][payload]
 *
 * Output list (guest → host):
 *   [4B: count]
 *   for each:
 *     [4B: dest_len][dest][1B: has_key]
 *       if has_key: [4B: key_len][key]
 *     [4B: payload_len][payload][4B: header_count]
 *       for each: [4B: key_len][key][4B: val_len][val]
 */

import { Event, Header, Output } from "./types";

/** Deserialize an Event from binary wire format. */
export function deserializeEvent(ptr: usize, len: usize): Event {
  let pos: usize = ptr;

  // UUID (16 bytes)
  const id = new Uint8Array(16);
  memory.copy(changetype<usize>(id.buffer), pos, 16);
  pos += 16;

  // Timestamp (i64 LE)
  const timestamp = load<i64>(pos);
  pos += 8;

  // Source string (length-prefixed)
  const srcLen = load<u32>(pos) as usize;
  pos += 4;
  const source = String.UTF8.decodeUnsafe(pos, srcLen);
  pos += srcLen;

  // Partition (u16 LE)
  const partition = load<u16>(pos);
  pos += 2;

  // Metadata
  const metaCount = load<u32>(pos) as i32;
  pos += 4;
  const metadata: Header[] = [];
  for (let i: i32 = 0; i < metaCount; i++) {
    const kl = load<u32>(pos) as usize;
    pos += 4;
    const k = String.UTF8.decodeUnsafe(pos, kl);
    pos += kl;
    const vl = load<u32>(pos) as usize;
    pos += 4;
    const v = String.UTF8.decodeUnsafe(pos, vl);
    pos += vl;
    metadata.push(new Header(k, v));
  }

  // Payload (length-prefixed)
  const payloadLen = load<u32>(pos) as usize;
  pos += 4;
  const payload = new Uint8Array(payloadLen as i32);
  memory.copy(changetype<usize>(payload.buffer), pos, payloadLen);

  return new Event(id, timestamp, source, partition, metadata, payload);
}

/** Serialize a list of outputs to binary wire format. Returns [ptr, len]. */
export function serializeOutputs(outputs: Output[]): usize {
  // Calculate total size
  let size: usize = 4; // output count
  for (let i = 0; i < outputs.length; i++) {
    const out = outputs[i];
    const destBuf = String.UTF8.encode(out.destination);
    size += 4 + destBuf.byteLength; // destination
    size += 1; // has_key flag
    if (out.key !== null) {
      size += 4 + out.key!.length; // key
    }
    size += 4 + out.payload.length; // payload
    size += 4; // header count
    for (let j = 0; j < out.headers.length; j++) {
      const hk = String.UTF8.encode(out.headers[j].key);
      const hv = String.UTF8.encode(out.headers[j].value);
      size += 4 + hk.byteLength + 4 + hv.byteLength;
    }
  }

  // Allocate: 4-byte length prefix + content
  const totalSize = 4 + size;
  const resultPtr = heap.alloc(totalSize);
  let pos = resultPtr;

  // Length prefix (content size)
  store<u32>(pos, size as u32);
  pos += 4;

  // Output count
  store<u32>(pos, outputs.length as u32);
  pos += 4;

  for (let i = 0; i < outputs.length; i++) {
    const out = outputs[i];

    // Destination
    const destBuf = String.UTF8.encode(out.destination);
    store<u32>(pos, destBuf.byteLength as u32);
    pos += 4;
    memory.copy(pos, changetype<usize>(destBuf), destBuf.byteLength);
    pos += destBuf.byteLength;

    // Key
    if (out.key !== null) {
      store<u8>(pos, 1);
      pos += 1;
      const key = out.key!;
      store<u32>(pos, key.length as u32);
      pos += 4;
      memory.copy(pos, changetype<usize>(key.buffer), key.length);
      pos += key.length;
    } else {
      store<u8>(pos, 0);
      pos += 1;
    }

    // Payload
    store<u32>(pos, out.payload.length as u32);
    pos += 4;
    memory.copy(pos, changetype<usize>(out.payload.buffer), out.payload.length);
    pos += out.payload.length;

    // Headers
    store<u32>(pos, out.headers.length as u32);
    pos += 4;
    for (let j = 0; j < out.headers.length; j++) {
      const hk = String.UTF8.encode(out.headers[j].key);
      store<u32>(pos, hk.byteLength as u32);
      pos += 4;
      memory.copy(pos, changetype<usize>(hk), hk.byteLength);
      pos += hk.byteLength;

      const hv = String.UTF8.encode(out.headers[j].value);
      store<u32>(pos, hv.byteLength as u32);
      pos += 4;
      memory.copy(pos, changetype<usize>(hv), hv.byteLength);
      pos += hv.byteLength;
    }
  }

  return resultPtr;
}
