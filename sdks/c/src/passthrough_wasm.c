/**
 * Passthrough Processor — T2 Wasm module for E2E testing.
 *
 * Exports the aeon-wasm ABI:
 *   alloc(size) -> ptr      — bump allocator
 *   dealloc(ptr, size)      — no-op (bump resets on alloc)
 *   process(ptr, len) -> ptr — returns pointer to [4B result_len LE][output_data]
 *
 * Same binary wire format as passthrough_processor.c (T1 native),
 * but adapted for the Wasm linear memory model.
 *
 * Compile: wasi-sdk clang --target=wasm32-unknown-unknown -nostdlib
 */

typedef unsigned char      uint8_t;
typedef unsigned short     uint16_t;
typedef unsigned int       uint32_t;
typedef unsigned long long uint64_t;

/* ── Bump allocator ──────────────────────────────────────────────── */

/* Heap starts at 128KB — past the data section (at ~64KB) and with room.
   Stack grows downward from initial-memory top (256KB). */
static unsigned int bump_ptr = 131072;

__attribute__((export_name("alloc")))
int alloc(int size) {
    /* Reset bump on each call — safe because host reads result
       before calling alloc again. */
    bump_ptr = 131072;
    int ptr = (int)bump_ptr;
    bump_ptr += (unsigned int)size;
    return ptr;
}

__attribute__((export_name("dealloc")))
void dealloc(int ptr, int size) {
    /* no-op for bump allocator */
    (void)ptr;
    (void)size;
}

/* ── Helpers ─────────────────────────────────────────────────────── */

static uint32_t read_u32_le(const uint8_t *p) {
    return (uint32_t)p[0] | ((uint32_t)p[1] << 8)
         | ((uint32_t)p[2] << 16) | ((uint32_t)p[3] << 24);
}

static void write_u32_le(uint8_t *p, uint32_t v) {
    p[0] = (uint8_t)(v & 0xFF);
    p[1] = (uint8_t)((v >> 8) & 0xFF);
    p[2] = (uint8_t)((v >> 16) & 0xFF);
    p[3] = (uint8_t)((v >> 24) & 0xFF);
}

static void memcpy_simple(uint8_t *dst, const uint8_t *src, uint32_t n) {
    for (uint32_t i = 0; i < n; i++) dst[i] = src[i];
}

/* ── Process function ────────────────────────────────────────────── */

__attribute__((export_name("process")))
int process(int ptr, int len) {
    const uint8_t *event_ptr = (const uint8_t *)(__SIZE_TYPE__)ptr;
    uint32_t event_len = (uint32_t)len;
    uint32_t off = 0;

    /* Skip UUID (16 bytes) */
    off += 16;

    /* Skip timestamp (8 bytes) */
    off += 8;

    /* Skip source: 4B len + N bytes */
    if (off + 4 > event_len) return 0;
    uint32_t src_len = read_u32_le(event_ptr + off); off += 4;
    off += src_len;

    /* Skip partition (2 bytes) */
    off += 2;

    /* Skip metadata entries */
    if (off + 4 > event_len) return 0;
    uint32_t meta_count = read_u32_le(event_ptr + off); off += 4;
    for (uint32_t i = 0; i < meta_count; i++) {
        if (off + 4 > event_len) return 0;
        uint32_t kl = read_u32_le(event_ptr + off); off += 4;
        off += kl;
        if (off + 4 > event_len) return 0;
        uint32_t vl = read_u32_le(event_ptr + off); off += 4;
        off += vl;
    }

    /* Read payload: 4B len + N bytes */
    if (off + 4 > event_len) return 0;
    uint32_t payload_len = read_u32_le(event_ptr + off); off += 4;
    const uint8_t *payload = event_ptr + off;

    /* Build output:
     * [4B output_count=1]
     * [4B dest_len=6][dest="output"][1B has_key=0]
     * [4B payload_len][payload]
     * [4B header_count=0]
     */
    uint32_t content_len = 4 + 4 + 6 + 1 + 4 + payload_len + 4; /* 23 + payload_len */
    uint32_t total = 4 + content_len; /* length prefix + content */

    /* Allocate output in bump memory */
    int out_ptr = (int)bump_ptr;
    bump_ptr += total;
    uint8_t *out = (uint8_t *)(__SIZE_TYPE__)out_ptr;

    /* Length prefix (content only, not including the 4B prefix itself) */
    write_u32_le(out, content_len);
    uint8_t *p = out + 4;

    /* output_count = 1 */
    write_u32_le(p, 1); p += 4;

    /* destination = "output" */
    write_u32_le(p, 6); p += 4;
    p[0] = 'o'; p[1] = 'u'; p[2] = 't'; p[3] = 'p'; p[4] = 'u'; p[5] = 't';
    p += 6;

    /* has_key = 0 */
    *p++ = 0;

    /* payload */
    write_u32_le(p, payload_len); p += 4;
    memcpy_simple(p, payload, payload_len); p += payload_len;

    /* header_count = 0 */
    write_u32_le(p, 0);

    return out_ptr;
}
