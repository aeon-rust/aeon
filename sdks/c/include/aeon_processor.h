/**
 * Aeon C/C++ Processor SDK
 *
 * Provides T1 (native .so/.dll) + T2 (Wasm) support via C-ABI,
 * plus wire format helpers for T3/T4 network transports.
 *
 * Pure C11, no external dependencies.
 */
#ifndef AEON_PROCESSOR_H
#define AEON_PROCESSOR_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ------------------------------------------------------------------ */
/*  Types                                                              */
/* ------------------------------------------------------------------ */

typedef struct {
    char     id[128];
    int64_t  timestamp;
    char     source[256];
    uint16_t partition;
    uint32_t metadata_count;
    char   (*metadata_keys)[256];
    char   (*metadata_values)[256];
    uint8_t *payload;
    uint32_t payload_len;
    int      has_source_offset;
    int64_t  source_offset;
} aeon_event_t;

typedef struct {
    char     destination[256];
    uint8_t *payload;
    uint32_t payload_len;
    uint8_t *key;
    uint32_t key_len;           /* 0 if no key */
    uint32_t header_count;
    char   (*header_keys)[256];
    char   (*header_values)[256];
    char     source_event_id[128];  /* empty string if not set */
    int      has_source_partition;
    int      source_partition;
    int      has_source_offset;
    int64_t  source_offset;
} aeon_output_t;

typedef struct {
    uint64_t       batch_id;
    uint32_t       event_count;
    aeon_event_t  *events;
} aeon_batch_request_t;

typedef struct {
    char     pipeline_name[256];
    uint16_t partition;
    uint8_t *batch_data;
    uint32_t batch_data_len;
} aeon_data_frame_t;

/* ------------------------------------------------------------------ */
/*  CRC32 (IEEE polynomial 0xEDB88320)                                 */
/* ------------------------------------------------------------------ */

uint32_t aeon_crc32(const uint8_t *data, size_t len);

/* ------------------------------------------------------------------ */
/*  Wire format functions                                              */
/* ------------------------------------------------------------------ */

/**
 * Decode batch request (JSON codec).
 * Wire format: [8B batch_id LE][4B count LE][per event: 4B len + json bytes][4B CRC32 LE]
 * Returns 0 on success, -1 on error.
 */
int aeon_decode_batch_request(const uint8_t *data, size_t len,
                              aeon_batch_request_t *out);

/**
 * Encode batch response. Caller must free returned buffer via aeon_free().
 * Response: [8B batch_id LE][4B count LE]
 *           [per event: 4B output_count LE [per output: 4B len + json bytes]]
 *           [4B CRC32 LE][64B zero signature]
 *
 * outputs_per_event[i] is an array of outputs for event i,
 * with counts in output_counts[i].
 */
uint8_t *aeon_encode_batch_response(
    uint64_t        batch_id,
    uint32_t        event_count,
    aeon_output_t **outputs_per_event,
    uint32_t       *output_counts,
    size_t         *out_len);

/**
 * Build data frame: [4B name_len LE][name UTF-8][2B partition LE][batch_data]
 * Caller must free returned buffer via aeon_free().
 */
uint8_t *aeon_build_data_frame(const char *pipeline_name,
                               uint16_t    partition,
                               const uint8_t *batch_data,
                               size_t      batch_data_len,
                               size_t     *out_len);

/**
 * Parse data frame routing header.
 * Returns 0 on success, -1 on error.
 * Note: out->batch_data points into the input buffer (no copy).
 */
int aeon_parse_data_frame(const uint8_t *data, size_t len,
                          aeon_data_frame_t *out);

/* ------------------------------------------------------------------ */
/*  Free functions                                                     */
/* ------------------------------------------------------------------ */

void aeon_free_batch_request(aeon_batch_request_t *req);
void aeon_free(void *ptr);

/* ------------------------------------------------------------------ */
/*  T1 C-ABI Processor Interface                                       */
/* ------------------------------------------------------------------ */
/*
 * A T1 native processor (.so / .dll) must export these symbols:
 *
 *   void*       aeon_processor_create(const uint8_t* config_ptr, size_t config_len);
 *   void        aeon_processor_destroy(void* ctx);
 *   int32_t     aeon_process(void* ctx,
 *                            const uint8_t* event_ptr, size_t event_len,
 *                            uint8_t* out_buf, size_t out_capacity, size_t* out_len);
 *   int32_t     aeon_process_batch(void* ctx,
 *                                  const uint8_t* events_ptr, size_t events_len,
 *                                  uint8_t* out_buf, size_t out_capacity, size_t* out_len);
 *   const char* aeon_processor_name(void);
 *   const char* aeon_processor_version(void);
 */

#ifdef _WIN32
#  define AEON_EXPORT __declspec(dllexport)
#else
#  define AEON_EXPORT __attribute__((visibility("default")))
#endif

/**
 * Helper macro for declaring T1 processor exports.
 * Usage:
 *   AEON_EXPORT_PROCESSOR(my_create, my_destroy, my_process,
 *                         my_process_batch, "my-proc", "1.0.0");
 */
#define AEON_EXPORT_PROCESSOR(create_fn, destroy_fn, process_fn,           \
                              process_batch_fn, name_str, version_str)     \
    AEON_EXPORT void* aeon_processor_create(const uint8_t* cfg, size_t l)  \
        { return create_fn(cfg, l); }                                      \
    AEON_EXPORT void  aeon_processor_destroy(void* ctx)                    \
        { destroy_fn(ctx); }                                               \
    AEON_EXPORT int32_t aeon_process(void* ctx,                            \
        const uint8_t* ep, size_t el,                                      \
        uint8_t* ob, size_t oc, size_t* ol)                                \
        { return process_fn(ctx, ep, el, ob, oc, ol); }                    \
    AEON_EXPORT int32_t aeon_process_batch(void* ctx,                      \
        const uint8_t* ep, size_t el,                                      \
        uint8_t* ob, size_t oc, size_t* ol)                                \
        { return process_batch_fn(ctx, ep, el, ob, oc, ol); }             \
    AEON_EXPORT const char* aeon_processor_name(void)                      \
        { return name_str; }                                               \
    AEON_EXPORT const char* aeon_processor_version(void)                   \
        { return version_str; }

#ifdef __cplusplus
}
#endif

#endif /* AEON_PROCESSOR_H */
