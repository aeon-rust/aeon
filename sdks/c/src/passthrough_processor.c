/**
 * Passthrough Processor — T1 native (.dll/.so) for E2E testing.
 *
 * Implements the aeon-native-sdk binary wire format:
 *
 * Event wire format (input):
 *   [16B UUID][8B timestamp LE][4B source_len LE][source UTF-8]
 *   [2B partition LE][4B meta_count LE][meta...][4B payload_len LE][payload]
 *
 * Output wire format (output):
 *   [4B output_count LE][per output:
 *     [4B dest_len LE][dest UTF-8][1B has_key(0)]
 *     [4B payload_len LE][payload][4B header_count LE(0)]]
 *
 * Each event produces exactly one output with destination="output"
 * and the same payload bytes as the input event.
 */

#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#  define EXPORT __declspec(dllexport)
#else
#  define EXPORT __attribute__((visibility("default")))
#endif

/* ── Endian helpers ─────────────────────────────────────────────── */

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

/* ── Processor context (stateless for passthrough) ──────────────── */

typedef struct {
    int dummy;
} PassthroughCtx;

/* ── C-ABI exports required by NativeProcessor ──────────────────── */

EXPORT void* aeon_processor_create(const uint8_t* config, size_t config_len) {
    (void)config; (void)config_len;
    PassthroughCtx *ctx = (PassthroughCtx*)malloc(sizeof(PassthroughCtx));
    if (ctx) ctx->dummy = 42;
    return ctx;
}

EXPORT void aeon_processor_destroy(void* ctx) {
    free(ctx);
}

/**
 * Process a single event: parse payload from wire format, produce one output.
 *
 * Returns 0 on success, -4 if buffer too small, -2 on parse error.
 */
EXPORT int32_t aeon_process(
    void*          ctx,
    const uint8_t* event_ptr,
    size_t         event_len,
    uint8_t*       out_buf,
    size_t         out_capacity,
    size_t*        out_len
) {
    (void)ctx;

    /* Parse event wire format to extract payload */
    size_t off = 0;

    /* UUID: 16 bytes */
    if (off + 16 > event_len) return -2;
    off += 16;

    /* Timestamp: 8 bytes */
    if (off + 8 > event_len) return -2;
    off += 8;

    /* Source: 4B len + N bytes */
    if (off + 4 > event_len) return -2;
    uint32_t src_len = read_u32_le(event_ptr + off); off += 4;
    if (off + src_len > event_len) return -2;
    off += src_len;

    /* Partition: 2 bytes */
    if (off + 2 > event_len) return -2;
    off += 2;

    /* Metadata: 4B count + per-entry (4B key_len + key + 4B val_len + val) */
    if (off + 4 > event_len) return -2;
    uint32_t meta_count = read_u32_le(event_ptr + off); off += 4;
    for (uint32_t i = 0; i < meta_count; i++) {
        if (off + 4 > event_len) return -2;
        uint32_t kl = read_u32_le(event_ptr + off); off += 4;
        if (off + kl > event_len) return -2;
        off += kl;
        if (off + 4 > event_len) return -2;
        uint32_t vl = read_u32_le(event_ptr + off); off += 4;
        if (off + vl > event_len) return -2;
        off += vl;
    }

    /* Payload: 4B len + N bytes */
    if (off + 4 > event_len) return -2;
    uint32_t payload_len = read_u32_le(event_ptr + off); off += 4;
    if (off + payload_len > event_len) return -2;
    const uint8_t *payload = event_ptr + off;

    /* Build output wire format:
     * [4B output_count=1]
     * [4B dest_len][dest="output"][1B has_key=0]
     * [4B payload_len][payload]
     * [4B header_count=0]
     */
    const char *dest = "output";
    uint32_t dest_len = 6; /* strlen("output") */

    size_t needed = 4               /* output_count */
                  + 4 + dest_len    /* destination */
                  + 1               /* has_key */
                  + 4 + payload_len /* payload */
                  + 4;              /* header_count */

    if (needed > out_capacity) return -4;

    uint8_t *p = out_buf;

    /* output_count = 1 */
    write_u32_le(p, 1); p += 4;

    /* destination */
    write_u32_le(p, dest_len); p += 4;
    memcpy(p, dest, dest_len); p += dest_len;

    /* has_key = 0 */
    *p++ = 0;

    /* payload */
    write_u32_le(p, payload_len); p += 4;
    memcpy(p, payload, payload_len); p += payload_len;

    /* header_count = 0 */
    write_u32_le(p, 0); p += 4;

    *out_len = (size_t)(p - out_buf);
    return 0;
}

/**
 * Process a batch of events.
 * Batch wire: [4B event_count][per event: [4B event_len][event_bytes]]
 * Output: concatenated outputs for all events (flat output wire format).
 */
EXPORT int32_t aeon_process_batch(
    void*          ctx,
    const uint8_t* events_ptr,
    size_t         events_len,
    uint8_t*       out_buf,
    size_t         out_capacity,
    size_t*        out_len
) {
    (void)ctx;

    size_t off = 0;
    if (off + 4 > events_len) return -2;
    uint32_t event_count = read_u32_le(events_ptr + off); off += 4;

    /* We'll build outputs incrementally.
     * Total output: [4B total_output_count][per output: ...]
     * For passthrough: total_output_count = event_count */

    size_t out_off = 0;
    if (out_off + 4 > out_capacity) return -4;
    write_u32_le(out_buf + out_off, event_count); out_off += 4;

    for (uint32_t i = 0; i < event_count; i++) {
        if (off + 4 > events_len) return -2;
        uint32_t ev_len = read_u32_le(events_ptr + off); off += 4;
        if (off + ev_len > events_len) return -2;

        const uint8_t *ev_ptr = events_ptr + off;
        off += ev_len;

        /* Parse this single event to extract payload */
        size_t eoff = 0;

        /* UUID: 16 bytes */
        if (eoff + 16 > ev_len) return -2;
        eoff += 16;

        /* Timestamp: 8 bytes */
        if (eoff + 8 > ev_len) return -2;
        eoff += 8;

        /* Source */
        if (eoff + 4 > ev_len) return -2;
        uint32_t sl = read_u32_le(ev_ptr + eoff); eoff += 4;
        if (eoff + sl > ev_len) return -2;
        eoff += sl;

        /* Partition: 2 bytes */
        if (eoff + 2 > ev_len) return -2;
        eoff += 2;

        /* Metadata */
        if (eoff + 4 > ev_len) return -2;
        uint32_t mc = read_u32_le(ev_ptr + eoff); eoff += 4;
        for (uint32_t m = 0; m < mc; m++) {
            if (eoff + 4 > ev_len) return -2;
            uint32_t kl = read_u32_le(ev_ptr + eoff); eoff += 4;
            if (eoff + kl > ev_len) return -2;
            eoff += kl;
            if (eoff + 4 > ev_len) return -2;
            uint32_t vl = read_u32_le(ev_ptr + eoff); eoff += 4;
            if (eoff + vl > ev_len) return -2;
            eoff += vl;
        }
        if (eoff + 4 > ev_len) return -2;
        uint32_t pl = read_u32_le(ev_ptr + eoff); eoff += 4;
        if (eoff + pl > ev_len) return -2;
        const uint8_t *payload = ev_ptr + eoff;

        /* Write one output entry (no count prefix) */
        size_t needed = 4 + 6 + 1 + 4 + pl + 4;
        if (out_off + needed > out_capacity) return -4;

        uint8_t *p = out_buf + out_off;
        write_u32_le(p, 6); p += 4;
        memcpy(p, "output", 6); p += 6;
        *p++ = 0;
        write_u32_le(p, pl); p += 4;
        memcpy(p, payload, pl); p += pl;
        write_u32_le(p, 0); p += 4;
        out_off = (size_t)(p - out_buf);
    }

    *out_len = out_off;
    return 0;
}

EXPORT const char* aeon_processor_name(void) {
    return "c-passthrough";
}

EXPORT const char* aeon_processor_version(void) {
    return "1.0.0";
}
