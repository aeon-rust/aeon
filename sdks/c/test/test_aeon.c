/**
 * Aeon C Processor SDK — Tests
 *
 * No external dependencies. Simple assert-style checks.
 */

#include "aeon_processor.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

static int tests_passed = 0;
static int tests_failed = 0;

#define TEST(name) printf("  %-50s ", #name)
#define PASS() do { printf("[PASS]\n"); tests_passed++; } while(0)
#define FAIL(msg) do { printf("[FAIL] %s\n", msg); tests_failed++; } while(0)
#define ASSERT(cond) if (!(cond)) { FAIL(#cond); return; }

/* ---- Endian helpers (duplicated for test wire building) ---- */
static void t_write_u16_le(uint8_t *buf, uint16_t v) {
    buf[0] = (uint8_t)(v & 0xFF);
    buf[1] = (uint8_t)((v >> 8) & 0xFF);
}

static void t_write_u32_le(uint8_t *buf, uint32_t v) {
    buf[0] = (uint8_t)(v & 0xFF);
    buf[1] = (uint8_t)((v >> 8) & 0xFF);
    buf[2] = (uint8_t)((v >> 16) & 0xFF);
    buf[3] = (uint8_t)((v >> 24) & 0xFF);
}

static void t_write_u64_le(uint8_t *buf, uint64_t v) {
    for (int i = 0; i < 8; i++)
        buf[i] = (uint8_t)((v >> (i * 8)) & 0xFF);
}

static uint32_t t_read_u32_le(const uint8_t *buf) {
    return (uint32_t)buf[0]
         | ((uint32_t)buf[1] << 8)
         | ((uint32_t)buf[2] << 16)
         | ((uint32_t)buf[3] << 24);
}

static uint64_t t_read_u64_le(const uint8_t *buf) {
    uint64_t v = 0;
    for (int i = 0; i < 8; i++)
        v |= (uint64_t)buf[i] << (i * 8);
    return v;
}

/* Build a batch request wire buffer with a single event JSON */
static uint8_t *build_batch_wire(uint64_t batch_id, const char *json,
                                 size_t json_len, size_t *out_len) {
    /* 8 + 4 + (4 + json_len) + 4 CRC */
    size_t total = 8 + 4 + 4 + json_len + 4;
    uint8_t *buf = (uint8_t *)calloc(1, total);
    t_write_u64_le(buf, batch_id);
    t_write_u32_le(buf + 8, 1); /* count = 1 */
    t_write_u32_le(buf + 12, (uint32_t)json_len);
    memcpy(buf + 16, json, json_len);
    uint32_t crc = aeon_crc32(buf, total - 4);
    t_write_u32_le(buf + total - 4, crc);
    *out_len = total;
    return buf;
}

/* Build a batch request with multiple event JSONs */
static uint8_t *build_multi_batch_wire(uint64_t batch_id,
                                       const char **jsons, size_t *json_lens,
                                       uint32_t count, size_t *out_len) {
    size_t total = 8 + 4 + 4; /* header + crc */
    for (uint32_t i = 0; i < count; i++)
        total += 4 + json_lens[i];
    uint8_t *buf = (uint8_t *)calloc(1, total);
    t_write_u64_le(buf, batch_id);
    t_write_u32_le(buf + 8, count);
    size_t off = 12;
    for (uint32_t i = 0; i < count; i++) {
        t_write_u32_le(buf + off, (uint32_t)json_lens[i]);
        off += 4;
        memcpy(buf + off, jsons[i], json_lens[i]);
        off += json_lens[i];
    }
    uint32_t crc = aeon_crc32(buf, total - 4);
    t_write_u32_le(buf + total - 4, crc);
    *out_len = total;
    return buf;
}

/* ================================================================== */
/*  CRC32 tests                                                        */
/* ================================================================== */

static void test_crc32_empty(void) {
    TEST(crc32_empty);
    uint32_t crc = aeon_crc32(NULL, 0);
    ASSERT(crc == 0x00000000);
    PASS();
}

static void test_crc32_check_value(void) {
    TEST(crc32_check_value);
    /* IEEE CRC32 check value for "123456789" is 0xCBF43926 */
    const uint8_t data[] = "123456789";
    uint32_t crc = aeon_crc32(data, 9);
    ASSERT(crc == 0xCBF43926);
    PASS();
}

static void test_crc32_binary_consistency(void) {
    TEST(crc32_binary_consistency);
    uint8_t data[] = { 0x00, 0xFF, 0x80, 0x7F, 0x01 };
    uint32_t crc1 = aeon_crc32(data, 5);
    uint32_t crc2 = aeon_crc32(data, 5);
    ASSERT(crc1 == crc2);
    ASSERT(crc1 != 0); /* Non-trivial data should not be zero */
    PASS();
}

/* ================================================================== */
/*  Wire format: data frame tests                                      */
/* ================================================================== */

static void test_data_frame_roundtrip(void) {
    TEST(data_frame_roundtrip);
    const uint8_t batch[] = { 0xDE, 0xAD, 0xBE, 0xEF };
    size_t frame_len;
    uint8_t *frame = aeon_build_data_frame("my-pipeline", 42,
                                           batch, sizeof(batch), &frame_len);
    ASSERT(frame != NULL);

    aeon_data_frame_t parsed;
    int rc = aeon_parse_data_frame(frame, frame_len, &parsed);
    ASSERT(rc == 0);
    ASSERT(strcmp(parsed.pipeline_name, "my-pipeline") == 0);
    ASSERT(parsed.partition == 42);
    ASSERT(parsed.batch_data_len == 4);
    ASSERT(memcmp(parsed.batch_data, batch, 4) == 0);

    aeon_free(frame);
    PASS();
}

static void test_data_frame_empty_name(void) {
    TEST(data_frame_empty_name);
    size_t frame_len;
    uint8_t *frame = aeon_build_data_frame("", 0, NULL, 0, &frame_len);
    ASSERT(frame != NULL);

    aeon_data_frame_t parsed;
    int rc = aeon_parse_data_frame(frame, frame_len, &parsed);
    ASSERT(rc == 0);
    ASSERT(strcmp(parsed.pipeline_name, "") == 0);
    ASSERT(parsed.partition == 0);
    ASSERT(parsed.batch_data_len == 0);

    aeon_free(frame);
    PASS();
}

static void test_data_frame_unicode_name(void) {
    TEST(data_frame_unicode_name);
    /* UTF-8 multi-byte name */
    const char *name = "pipeline-\xC3\xA9\xC3\xA0";  /* e-acute, a-grave */
    size_t frame_len;
    uint8_t *frame = aeon_build_data_frame(name, 100, NULL, 0, &frame_len);
    ASSERT(frame != NULL);

    aeon_data_frame_t parsed;
    int rc = aeon_parse_data_frame(frame, frame_len, &parsed);
    ASSERT(rc == 0);
    ASSERT(strcmp(parsed.pipeline_name, name) == 0);
    ASSERT(parsed.partition == 100);

    aeon_free(frame);
    PASS();
}

static void test_data_frame_too_short(void) {
    TEST(data_frame_too_short);
    uint8_t data[] = { 0x01, 0x00, 0x00 }; /* Only 3 bytes */
    aeon_data_frame_t parsed;
    int rc = aeon_parse_data_frame(data, 3, &parsed);
    ASSERT(rc == -1);
    PASS();
}

static void test_batch_response_structure(void) {
    TEST(batch_response_structure);
    /* Build a response with 1 event, 1 output */
    aeon_output_t out;
    memset(&out, 0, sizeof(out));
    strcpy(out.destination, "sink-topic");
    uint8_t pay[] = "hello";
    out.payload = pay;
    out.payload_len = 5;

    aeon_output_t *outputs_arr = &out;
    uint32_t count = 1;
    size_t resp_len;
    uint8_t *resp = aeon_encode_batch_response(99, 1, &outputs_arr, &count, &resp_len);
    ASSERT(resp != NULL);

    /* Check header */
    ASSERT(t_read_u64_le(resp) == 99);           /* batch_id */
    ASSERT(t_read_u32_le(resp + 8) == 1);         /* event_count */
    /* Check trailing 64-byte zero signature */
    ASSERT(resp_len >= 64);
    int sig_ok = 1;
    for (int i = 0; i < 64; i++) {
        if (resp[resp_len - 64 + i] != 0) { sig_ok = 0; break; }
    }
    ASSERT(sig_ok);

    aeon_free(resp);
    PASS();
}

/* ================================================================== */
/*  Batch roundtrip tests                                              */
/* ================================================================== */

static void test_batch_single_event(void) {
    TEST(batch_single_event);
    /* "SGVsbG8=" is base64 for "Hello" */
    const char *json =
        "{\"id\":\"evt-001\",\"timestamp\":1700000000,"
        "\"source\":\"test-src\",\"partition\":3,"
        "\"metadata\":{\"k1\":\"v1\"},"
        "\"payload\":\"SGVsbG8=\"}";
    size_t wire_len;
    uint8_t *wire = build_batch_wire(42, json, strlen(json), &wire_len);

    aeon_batch_request_t req;
    int rc = aeon_decode_batch_request(wire, wire_len, &req);
    ASSERT(rc == 0);
    ASSERT(req.batch_id == 42);
    ASSERT(req.event_count == 1);
    ASSERT(strcmp(req.events[0].id, "evt-001") == 0);
    ASSERT(req.events[0].timestamp == 1700000000);
    ASSERT(strcmp(req.events[0].source, "test-src") == 0);
    ASSERT(req.events[0].partition == 3);
    ASSERT(req.events[0].metadata_count == 1);
    ASSERT(strcmp(req.events[0].metadata_keys[0], "k1") == 0);
    ASSERT(strcmp(req.events[0].metadata_values[0], "v1") == 0);
    ASSERT(req.events[0].payload_len == 5);
    ASSERT(memcmp(req.events[0].payload, "Hello", 5) == 0);

    aeon_free_batch_request(&req);
    free(wire);
    PASS();
}

static void test_batch_crc_mismatch(void) {
    TEST(batch_crc_mismatch);
    const char *json = "{\"id\":\"x\",\"timestamp\":0,\"source\":\"s\","
                       "\"partition\":0,\"metadata\":{},\"payload\":\"\"}";
    size_t wire_len;
    uint8_t *wire = build_batch_wire(1, json, strlen(json), &wire_len);

    /* Corrupt the CRC */
    wire[wire_len - 1] ^= 0xFF;

    aeon_batch_request_t req;
    int rc = aeon_decode_batch_request(wire, wire_len, &req);
    ASSERT(rc == -1);

    free(wire);
    PASS();
}

static void test_batch_multiple_events(void) {
    TEST(batch_multiple_events);
    const char *j1 = "{\"id\":\"e1\",\"timestamp\":100,\"source\":\"s1\","
                     "\"partition\":0,\"metadata\":{},\"payload\":\"\"}";
    const char *j2 = "{\"id\":\"e2\",\"timestamp\":200,\"source\":\"s2\","
                     "\"partition\":1,\"metadata\":{},\"payload\":\"\"}";
    const char *jsons[] = { j1, j2 };
    size_t lens[] = { strlen(j1), strlen(j2) };
    size_t wire_len;
    uint8_t *wire = build_multi_batch_wire(77, jsons, lens, 2, &wire_len);

    aeon_batch_request_t req;
    int rc = aeon_decode_batch_request(wire, wire_len, &req);
    ASSERT(rc == 0);
    ASSERT(req.batch_id == 77);
    ASSERT(req.event_count == 2);
    ASSERT(strcmp(req.events[0].id, "e1") == 0);
    ASSERT(req.events[0].timestamp == 100);
    ASSERT(strcmp(req.events[1].id, "e2") == 0);
    ASSERT(req.events[1].timestamp == 200);
    ASSERT(req.events[1].partition == 1);

    aeon_free_batch_request(&req);
    free(wire);
    PASS();
}

/* ================================================================== */
/*  Types tests                                                        */
/* ================================================================== */

static void test_event_init(void) {
    TEST(event_init);
    aeon_event_t ev;
    memset(&ev, 0, sizeof(ev));
    strcpy(ev.id, "test-id");
    ev.timestamp = 1234567890;
    strcpy(ev.source, "my-source");
    ev.partition = 7;
    ASSERT(strcmp(ev.id, "test-id") == 0);
    ASSERT(ev.timestamp == 1234567890);
    ASSERT(ev.partition == 7);
    ASSERT(ev.payload_len == 0);
    PASS();
}

static void test_output_init(void) {
    TEST(output_init);
    aeon_output_t out;
    memset(&out, 0, sizeof(out));
    strcpy(out.destination, "sink-topic");
    ASSERT(strcmp(out.destination, "sink-topic") == 0);
    ASSERT(out.payload_len == 0);
    ASSERT(out.key_len == 0);
    ASSERT(out.header_count == 0);
    ASSERT(out.source_event_id[0] == '\0');
    PASS();
}

static void test_output_with_identity(void) {
    TEST(output_with_identity);
    aeon_output_t out;
    memset(&out, 0, sizeof(out));
    strcpy(out.destination, "dest");
    strcpy(out.source_event_id, "evt-123");
    out.has_source_partition = 1;
    out.source_partition = 5;
    out.has_source_offset = 1;
    out.source_offset = 999;
    ASSERT(strcmp(out.source_event_id, "evt-123") == 0);
    ASSERT(out.has_source_partition == 1);
    ASSERT(out.source_partition == 5);
    ASSERT(out.source_offset == 999);
    PASS();
}

static void test_payload_string(void) {
    TEST(payload_string);
    aeon_event_t ev;
    memset(&ev, 0, sizeof(ev));
    uint8_t data[] = "Hello, Aeon!";
    ev.payload = data;
    ev.payload_len = (uint32_t)strlen((char *)data);
    ASSERT(ev.payload_len == 12);
    ASSERT(memcmp(ev.payload, "Hello, Aeon!", 12) == 0);
    PASS();
}

/* ================================================================== */
/*  JSON codec tests                                                   */
/* ================================================================== */

static void test_json_event_roundtrip(void) {
    TEST(json_event_roundtrip);
    /* Build a wire batch with a known event, decode, re-encode as batch response,
       then verify we can decode the event fields. */
    const char *json =
        "{\"id\":\"rt-001\",\"timestamp\":555,"
        "\"source\":\"src\",\"partition\":2,"
        "\"metadata\":{\"a\":\"b\",\"c\":\"d\"},"
        "\"payload\":\"AQID\",\"source_offset\":42}";
    /* AQID = base64(0x01, 0x02, 0x03) */
    size_t wire_len;
    uint8_t *wire = build_batch_wire(10, json, strlen(json), &wire_len);

    aeon_batch_request_t req;
    int rc = aeon_decode_batch_request(wire, wire_len, &req);
    ASSERT(rc == 0);
    ASSERT(strcmp(req.events[0].id, "rt-001") == 0);
    ASSERT(req.events[0].timestamp == 555);
    ASSERT(req.events[0].partition == 2);
    ASSERT(req.events[0].metadata_count == 2);
    ASSERT(req.events[0].has_source_offset == 1);
    ASSERT(req.events[0].source_offset == 42);
    ASSERT(req.events[0].payload_len == 3);
    ASSERT(req.events[0].payload[0] == 0x01);
    ASSERT(req.events[0].payload[1] == 0x02);
    ASSERT(req.events[0].payload[2] == 0x03);

    aeon_free_batch_request(&req);
    free(wire);
    PASS();
}

static void test_json_output_roundtrip(void) {
    TEST(json_output_roundtrip);
    /* Encode an output via batch response, then check structure */
    aeon_output_t out;
    memset(&out, 0, sizeof(out));
    strcpy(out.destination, "out-topic");
    uint8_t pay[] = { 0xCA, 0xFE };
    out.payload = pay;
    out.payload_len = 2;
    uint8_t key_data[] = "mykey";
    out.key = key_data;
    out.key_len = 5;
    strcpy(out.source_event_id, "src-evt-1");
    out.has_source_partition = 1;
    out.source_partition = 3;

    aeon_output_t *arr = &out;
    uint32_t oc = 1;
    size_t resp_len;
    uint8_t *resp = aeon_encode_batch_response(50, 1, &arr, &oc, &resp_len);
    ASSERT(resp != NULL);
    ASSERT(resp_len > 12 + 4 + 4 + 4 + 64); /* Must have content */

    /* Verify batch_id in response */
    ASSERT(t_read_u64_le(resp) == 50);

    aeon_free(resp);
    PASS();
}

static void test_json_optional_fields(void) {
    TEST(json_optional_fields);
    /* Event without source_offset */
    const char *json =
        "{\"id\":\"opt-1\",\"timestamp\":0,"
        "\"source\":\"s\",\"partition\":0,"
        "\"metadata\":{},\"payload\":\"\"}";
    size_t wire_len;
    uint8_t *wire = build_batch_wire(1, json, strlen(json), &wire_len);

    aeon_batch_request_t req;
    int rc = aeon_decode_batch_request(wire, wire_len, &req);
    ASSERT(rc == 0);
    ASSERT(req.events[0].has_source_offset == 0);
    ASSERT(req.events[0].metadata_count == 0);
    ASSERT(req.events[0].payload_len == 0);

    aeon_free_batch_request(&req);
    free(wire);
    PASS();
}

static void test_json_binary_payload(void) {
    TEST(json_binary_payload);
    /* All 256 byte values encoded: test with a few bytes */
    /* Base64 of {0x00, 0xFF, 0x80} = "AP+A" */
    const char *json =
        "{\"id\":\"bin\",\"timestamp\":0,"
        "\"source\":\"s\",\"partition\":0,"
        "\"metadata\":{},\"payload\":\"AP+A\"}";
    size_t wire_len;
    uint8_t *wire = build_batch_wire(1, json, strlen(json), &wire_len);

    aeon_batch_request_t req;
    int rc = aeon_decode_batch_request(wire, wire_len, &req);
    ASSERT(rc == 0);
    ASSERT(req.events[0].payload_len == 3);
    ASSERT(req.events[0].payload[0] == 0x00);
    ASSERT(req.events[0].payload[1] == 0xFF);
    ASSERT(req.events[0].payload[2] == 0x80);

    aeon_free_batch_request(&req);
    free(wire);
    PASS();
}

static void test_base64_correctness(void) {
    TEST(base64_correctness);
    /* "SGVsbG8gV29ybGQ=" = "Hello World" */
    const char *json =
        "{\"id\":\"b64\",\"timestamp\":0,"
        "\"source\":\"s\",\"partition\":0,"
        "\"metadata\":{},\"payload\":\"SGVsbG8gV29ybGQ=\"}";
    size_t wire_len;
    uint8_t *wire = build_batch_wire(1, json, strlen(json), &wire_len);

    aeon_batch_request_t req;
    int rc = aeon_decode_batch_request(wire, wire_len, &req);
    ASSERT(rc == 0);
    ASSERT(req.events[0].payload_len == 11);
    ASSERT(memcmp(req.events[0].payload, "Hello World", 11) == 0);

    aeon_free_batch_request(&req);
    free(wire);
    PASS();
}

/* ================================================================== */
/*  Cross-validation tests                                             */
/* ================================================================== */

static void test_crc_on_response(void) {
    TEST(crc_on_response_matches);
    aeon_output_t out;
    memset(&out, 0, sizeof(out));
    strcpy(out.destination, "d");
    uint8_t pay[] = "x";
    out.payload = pay;
    out.payload_len = 1;

    aeon_output_t *arr = &out;
    uint32_t oc = 1;
    size_t resp_len;
    uint8_t *resp = aeon_encode_batch_response(1, 1, &arr, &oc, &resp_len);
    ASSERT(resp != NULL);

    /* CRC is 4 bytes before the 64-byte signature */
    size_t crc_offset = resp_len - 64 - 4;
    uint32_t stored_crc = t_read_u32_le(resp + crc_offset);
    uint32_t computed_crc = aeon_crc32(resp, crc_offset);
    ASSERT(stored_crc == computed_crc);

    aeon_free(resp);
    PASS();
}

static void test_response_structure_valid(void) {
    TEST(response_structure_valid);
    /* 2 events, first with 1 output, second with 2 outputs */
    aeon_output_t o1;
    memset(&o1, 0, sizeof(o1));
    strcpy(o1.destination, "d1");
    uint8_t p1[] = "a";
    o1.payload = p1;
    o1.payload_len = 1;

    aeon_output_t o2a, o2b;
    memset(&o2a, 0, sizeof(o2a));
    memset(&o2b, 0, sizeof(o2b));
    strcpy(o2a.destination, "d2a");
    uint8_t p2a[] = "b";
    o2a.payload = p2a;
    o2a.payload_len = 1;
    strcpy(o2b.destination, "d2b");
    uint8_t p2b[] = "c";
    o2b.payload = p2b;
    o2b.payload_len = 1;

    aeon_output_t *e1_outs = &o1;
    aeon_output_t e2_outs_arr[2];
    e2_outs_arr[0] = o2a;
    e2_outs_arr[1] = o2b;
    aeon_output_t *e2_outs = e2_outs_arr;

    aeon_output_t *per_event[2];
    per_event[0] = e1_outs;
    per_event[1] = e2_outs;
    uint32_t counts[2] = { 1, 2 };

    size_t resp_len;
    uint8_t *resp = aeon_encode_batch_response(500, 2, per_event, counts, &resp_len);
    ASSERT(resp != NULL);

    /* Verify header */
    ASSERT(t_read_u64_le(resp) == 500);    /* batch_id */
    ASSERT(t_read_u32_le(resp + 8) == 2);  /* event_count */

    /* First event: output_count = 1 */
    ASSERT(t_read_u32_le(resp + 12) == 1);

    /* Verify it ends with 64 zero bytes */
    int sig_ok = 1;
    for (int i = 0; i < 64; i++) {
        if (resp[resp_len - 64 + i] != 0) { sig_ok = 0; break; }
    }
    ASSERT(sig_ok);

    /* Verify CRC */
    size_t crc_off = resp_len - 64 - 4;
    uint32_t stored = t_read_u32_le(resp + crc_off);
    uint32_t computed = aeon_crc32(resp, crc_off);
    ASSERT(stored == computed);

    aeon_free(resp);
    PASS();
}

/* ================================================================== */
/*  Main                                                               */
/* ================================================================== */

int main(void) {
    printf("\n=== Aeon C Processor SDK Tests ===\n\n");

    printf("[CRC32]\n");
    test_crc32_empty();
    test_crc32_check_value();
    test_crc32_binary_consistency();

    printf("\n[Wire Encode/Decode]\n");
    test_data_frame_roundtrip();
    test_data_frame_empty_name();
    test_data_frame_unicode_name();
    test_data_frame_too_short();
    test_batch_response_structure();

    printf("\n[Batch Roundtrip]\n");
    test_batch_single_event();
    test_batch_crc_mismatch();
    test_batch_multiple_events();

    printf("\n[Types]\n");
    test_event_init();
    test_output_init();
    test_output_with_identity();
    test_payload_string();

    printf("\n[JSON Codec]\n");
    test_json_event_roundtrip();
    test_json_output_roundtrip();
    test_json_optional_fields();
    test_json_binary_payload();
    test_base64_correctness();

    printf("\n[Cross Validation]\n");
    test_crc_on_response();
    test_response_structure_valid();

    printf("\n=== Results: %d passed, %d failed ===\n\n",
           tests_passed, tests_failed);

    return tests_failed > 0 ? 1 : 0;
}
