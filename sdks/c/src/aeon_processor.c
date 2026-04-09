/**
 * Aeon C Processor SDK — Implementation
 *
 * Pure C11, no external dependencies.
 * All wire format integers are LITTLE-ENDIAN (explicit byte ops).
 */

#include "aeon_processor.h"

#include <stdlib.h>
#include <string.h>
#include <stdio.h>

/* ================================================================== */
/*  Endian helpers (portable — never rely on system endianness)        */
/* ================================================================== */

static void write_u16_le(uint8_t *buf, uint16_t v) {
    buf[0] = (uint8_t)(v & 0xFF);
    buf[1] = (uint8_t)((v >> 8) & 0xFF);
}

static void write_u32_le(uint8_t *buf, uint32_t v) {
    buf[0] = (uint8_t)(v & 0xFF);
    buf[1] = (uint8_t)((v >> 8) & 0xFF);
    buf[2] = (uint8_t)((v >> 16) & 0xFF);
    buf[3] = (uint8_t)((v >> 24) & 0xFF);
}

static void write_u64_le(uint8_t *buf, uint64_t v) {
    for (int i = 0; i < 8; i++) {
        buf[i] = (uint8_t)((v >> (i * 8)) & 0xFF);
    }
}

static uint16_t read_u16_le(const uint8_t *buf) {
    return (uint16_t)buf[0] | ((uint16_t)buf[1] << 8);
}

static uint32_t read_u32_le(const uint8_t *buf) {
    return (uint32_t)buf[0]
         | ((uint32_t)buf[1] << 8)
         | ((uint32_t)buf[2] << 16)
         | ((uint32_t)buf[3] << 24);
}

static uint64_t read_u64_le(const uint8_t *buf) {
    uint64_t v = 0;
    for (int i = 0; i < 8; i++) {
        v |= (uint64_t)buf[i] << (i * 8);
    }
    return v;
}

/* ================================================================== */
/*  CRC32 — IEEE polynomial 0xEDB88320                                 */
/* ================================================================== */

static uint32_t crc32_table[256];
static int      crc32_table_init = 0;

static void crc32_init_table(void) {
    for (uint32_t i = 0; i < 256; i++) {
        uint32_t c = i;
        for (int j = 0; j < 8; j++) {
            if (c & 1)
                c = 0xEDB88320u ^ (c >> 1);
            else
                c >>= 1;
        }
        crc32_table[i] = c;
    }
    crc32_table_init = 1;
}

uint32_t aeon_crc32(const uint8_t *data, size_t len) {
    if (!crc32_table_init) crc32_init_table();
    uint32_t crc = 0xFFFFFFFFu;
    for (size_t i = 0; i < len; i++) {
        crc = crc32_table[(crc ^ data[i]) & 0xFF] ^ (crc >> 8);
    }
    return crc ^ 0xFFFFFFFFu;
}

/* ================================================================== */
/*  Base64 encode / decode                                             */
/* ================================================================== */

static const char b64_chars[] =
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

static char *base64_encode(const uint8_t *data, size_t len, size_t *out_len) {
    size_t olen = 4 * ((len + 2) / 3);
    char *out = (char *)malloc(olen + 1);
    if (!out) return NULL;

    size_t i = 0, j = 0;
    while (i < len) {
        uint32_t a = (i < len) ? data[i++] : 0;
        uint32_t b = (i < len) ? data[i++] : 0;
        uint32_t c = (i < len) ? data[i++] : 0;
        uint32_t triple = (a << 16) | (b << 8) | c;

        out[j++] = b64_chars[(triple >> 18) & 0x3F];
        out[j++] = b64_chars[(triple >> 12) & 0x3F];
        out[j++] = b64_chars[(triple >> 6) & 0x3F];
        out[j++] = b64_chars[triple & 0x3F];
    }

    /* Padding */
    size_t mod = len % 3;
    if (mod == 1) {
        out[j - 1] = '=';
        out[j - 2] = '=';
    } else if (mod == 2) {
        out[j - 1] = '=';
    }

    out[j] = '\0';
    if (out_len) *out_len = j;
    return out;
}

static int b64_decode_char(char c) {
    if (c >= 'A' && c <= 'Z') return c - 'A';
    if (c >= 'a' && c <= 'z') return c - 'a' + 26;
    if (c >= '0' && c <= '9') return c - '0' + 52;
    if (c == '+') return 62;
    if (c == '/') return 63;
    return -1;
}

static uint8_t *base64_decode(const char *data, size_t len, size_t *out_len) {
    if (len == 0 || len % 4 != 0) {
        /* Handle zero-length */
        if (len == 0) {
            uint8_t *out = (uint8_t *)malloc(1);
            if (out_len) *out_len = 0;
            return out;
        }
        return NULL;
    }

    size_t olen = (len / 4) * 3;
    if (data[len - 1] == '=') olen--;
    if (data[len - 2] == '=') olen--;

    uint8_t *out = (uint8_t *)malloc(olen + 1);
    if (!out) return NULL;

    size_t j = 0;
    for (size_t i = 0; i < len; i += 4) {
        int a = b64_decode_char(data[i]);
        int b = b64_decode_char(data[i + 1]);
        int c = (data[i + 2] == '=') ? 0 : b64_decode_char(data[i + 2]);
        int d = (data[i + 3] == '=') ? 0 : b64_decode_char(data[i + 3]);

        if (a < 0 || b < 0 || c < 0 || d < 0) {
            free(out);
            return NULL;
        }

        uint32_t triple = ((uint32_t)a << 18) | ((uint32_t)b << 12)
                        | ((uint32_t)c << 6) | (uint32_t)d;

        if (j < olen) out[j++] = (uint8_t)((triple >> 16) & 0xFF);
        if (j < olen) out[j++] = (uint8_t)((triple >> 8) & 0xFF);
        if (j < olen) out[j++] = (uint8_t)(triple & 0xFF);
    }

    if (out_len) *out_len = olen;
    return out;
}

/* ================================================================== */
/*  Minimal JSON helpers                                               */
/* ================================================================== */

/* Dynamic buffer for building JSON strings */
typedef struct {
    uint8_t *data;
    size_t   len;
    size_t   cap;
} jbuf_t;

static void jbuf_init(jbuf_t *b) {
    b->cap = 512;
    b->data = (uint8_t *)malloc(b->cap);
    b->len = 0;
}

static void jbuf_ensure(jbuf_t *b, size_t extra) {
    while (b->len + extra > b->cap) {
        b->cap *= 2;
        b->data = (uint8_t *)realloc(b->data, b->cap);
    }
}

static void jbuf_append(jbuf_t *b, const char *s, size_t n) {
    jbuf_ensure(b, n);
    memcpy(b->data + b->len, s, n);
    b->len += n;
}

static void jbuf_str(jbuf_t *b, const char *s) {
    jbuf_append(b, s, strlen(s));
}

static void jbuf_char(jbuf_t *b, char c) {
    jbuf_ensure(b, 1);
    b->data[b->len++] = (uint8_t)c;
}

/* Write a JSON-escaped string (with surrounding quotes) */
static void jbuf_json_string(jbuf_t *b, const char *s) {
    jbuf_char(b, '"');
    while (*s) {
        char c = *s++;
        switch (c) {
            case '"':  jbuf_str(b, "\\\""); break;
            case '\\': jbuf_str(b, "\\\\"); break;
            case '\n': jbuf_str(b, "\\n");  break;
            case '\r': jbuf_str(b, "\\r");  break;
            case '\t': jbuf_str(b, "\\t");  break;
            default:
                if ((unsigned char)c < 0x20) {
                    char esc[8];
                    snprintf(esc, sizeof(esc), "\\u%04x", (unsigned)c);
                    jbuf_str(b, esc);
                } else {
                    jbuf_char(b, c);
                }
                break;
        }
    }
    jbuf_char(b, '"');
}

static void jbuf_i64(jbuf_t *b, int64_t v) {
    char tmp[32];
    snprintf(tmp, sizeof(tmp), "%lld", (long long)v);
    jbuf_str(b, tmp);
}

static void jbuf_u32(jbuf_t *b, uint32_t v) {
    char tmp[16];
    snprintf(tmp, sizeof(tmp), "%u", (unsigned)v);
    jbuf_str(b, tmp);
}

/* ------------------------------------------------------------------ */
/*  Minimal JSON parser (state-machine, no recursion needed)           */
/* ------------------------------------------------------------------ */

/* Skip whitespace */
static const char *json_skip_ws(const char *p, const char *end) {
    while (p < end && (*p == ' ' || *p == '\t' || *p == '\n' || *p == '\r'))
        p++;
    return p;
}

/* Read a JSON string value (after opening quote).
   Writes unescaped content to dst (up to dst_cap-1), null-terminates.
   Returns pointer past the closing quote, or NULL on error. */
static const char *json_read_string(const char *p, const char *end,
                                    char *dst, size_t dst_cap) {
    size_t di = 0;
    while (p < end && *p != '"') {
        if (*p == '\\') {
            p++;
            if (p >= end) return NULL;
            char c;
            switch (*p) {
                case '"':  c = '"';  break;
                case '\\': c = '\\'; break;
                case '/':  c = '/';  break;
                case 'n':  c = '\n'; break;
                case 'r':  c = '\r'; break;
                case 't':  c = '\t'; break;
                case 'u':
                    /* Skip unicode escapes — store '?' placeholder */
                    p += 4;
                    c = '?';
                    break;
                default:   c = *p;   break;
            }
            if (di < dst_cap - 1) dst[di++] = c;
            p++;
        } else {
            if (di < dst_cap - 1) dst[di++] = *p;
            p++;
        }
    }
    dst[di] = '\0';
    if (p >= end) return NULL;
    return p + 1; /* skip closing quote */
}

/* Read a JSON number (integer). Returns pointer past the number. */
static const char *json_read_i64(const char *p, const char *end, int64_t *out) {
    char tmp[32];
    int i = 0;
    if (p < end && *p == '-') tmp[i++] = *p++;
    while (p < end && *p >= '0' && *p <= '9' && i < 30)
        tmp[i++] = *p++;
    tmp[i] = '\0';
    *out = 0;
    /* Parse manually for portability */
    int neg = 0;
    int si = 0;
    if (tmp[0] == '-') { neg = 1; si = 1; }
    int64_t v = 0;
    for (int j = si; tmp[j]; j++) {
        v = v * 10 + (tmp[j] - '0');
    }
    *out = neg ? -v : v;
    return p;
}

/* Skip a JSON value (string, number, object, array, bool, null) */
static const char *json_skip_value(const char *p, const char *end) {
    p = json_skip_ws(p, end);
    if (p >= end) return NULL;

    if (*p == '"') {
        /* String */
        p++;
        while (p < end && *p != '"') {
            if (*p == '\\') p++;
            p++;
        }
        return (p < end) ? p + 1 : NULL;
    }
    if (*p == '{') {
        /* Object */
        int depth = 1;
        p++;
        while (p < end && depth > 0) {
            if (*p == '"') {
                p++;
                while (p < end && *p != '"') {
                    if (*p == '\\') p++;
                    p++;
                }
                if (p < end) p++;
            } else {
                if (*p == '{') depth++;
                else if (*p == '}') depth--;
                p++;
            }
        }
        return p;
    }
    if (*p == '[') {
        /* Array */
        int depth = 1;
        p++;
        while (p < end && depth > 0) {
            if (*p == '"') {
                p++;
                while (p < end && *p != '"') {
                    if (*p == '\\') p++;
                    p++;
                }
                if (p < end) p++;
            } else {
                if (*p == '[') depth++;
                else if (*p == ']') depth--;
                p++;
            }
        }
        return p;
    }
    /* number, bool, null */
    while (p < end && *p != ',' && *p != '}' && *p != ']'
           && *p != ' ' && *p != '\n' && *p != '\r' && *p != '\t')
        p++;
    return p;
}

/* ================================================================== */
/*  JSON encode Event                                                  */
/* ================================================================== */

static uint8_t *json_encode_event(const aeon_event_t *ev, size_t *out_len) {
    jbuf_t b;
    jbuf_init(&b);

    jbuf_char(&b, '{');

    /* id */
    jbuf_str(&b, "\"id\":");
    jbuf_json_string(&b, ev->id);

    /* timestamp */
    jbuf_str(&b, ",\"timestamp\":");
    jbuf_i64(&b, ev->timestamp);

    /* source */
    jbuf_str(&b, ",\"source\":");
    jbuf_json_string(&b, ev->source);

    /* partition */
    jbuf_str(&b, ",\"partition\":");
    jbuf_u32(&b, ev->partition);

    /* metadata */
    jbuf_str(&b, ",\"metadata\":{");
    for (uint32_t i = 0; i < ev->metadata_count; i++) {
        if (i > 0) jbuf_char(&b, ',');
        jbuf_json_string(&b, ev->metadata_keys[i]);
        jbuf_char(&b, ':');
        jbuf_json_string(&b, ev->metadata_values[i]);
    }
    jbuf_char(&b, '}');

    /* payload (base64) */
    jbuf_str(&b, ",\"payload\":\"");
    if (ev->payload_len > 0) {
        size_t b64len;
        char *b64 = base64_encode(ev->payload, ev->payload_len, &b64len);
        if (b64) {
            jbuf_append(&b, b64, b64len);
            free(b64);
        }
    }
    jbuf_char(&b, '"');

    /* source_offset */
    if (ev->has_source_offset) {
        jbuf_str(&b, ",\"source_offset\":");
        jbuf_i64(&b, ev->source_offset);
    }

    jbuf_char(&b, '}');

    *out_len = b.len;
    return b.data;
}

/* ================================================================== */
/*  JSON decode Event                                                  */
/* ================================================================== */

static int json_decode_event(const uint8_t *data, size_t len, aeon_event_t *ev) {
    memset(ev, 0, sizeof(*ev));

    const char *p = (const char *)data;
    const char *end = p + len;

    p = json_skip_ws(p, end);
    if (p >= end || *p != '{') return -1;
    p++;

    while (p < end) {
        p = json_skip_ws(p, end);
        if (p >= end) return -1;
        if (*p == '}') break;
        if (*p == ',') { p++; continue; }

        /* Key */
        if (*p != '"') return -1;
        p++;
        char key[64];
        p = json_read_string(p, end, key, sizeof(key));
        if (!p) return -1;

        p = json_skip_ws(p, end);
        if (p >= end || *p != ':') return -1;
        p++;
        p = json_skip_ws(p, end);
        if (p >= end) return -1;

        if (strcmp(key, "id") == 0) {
            if (*p != '"') return -1;
            p++;
            p = json_read_string(p, end, ev->id, sizeof(ev->id));
            if (!p) return -1;
        } else if (strcmp(key, "timestamp") == 0) {
            p = json_read_i64(p, end, &ev->timestamp);
        } else if (strcmp(key, "source") == 0) {
            if (*p != '"') return -1;
            p++;
            p = json_read_string(p, end, ev->source, sizeof(ev->source));
            if (!p) return -1;
        } else if (strcmp(key, "partition") == 0) {
            int64_t tmp;
            p = json_read_i64(p, end, &tmp);
            ev->partition = (uint16_t)tmp;
        } else if (strcmp(key, "payload") == 0) {
            if (*p != '"') return -1;
            p++;
            /* Read base64 string into temp buffer */
            char *b64buf = (char *)malloc(len + 1);
            const char *np = json_read_string(p, end, b64buf, len + 1);
            if (!np) { free(b64buf); return -1; }
            p = np;
            size_t b64slen = strlen(b64buf);
            if (b64slen > 0) {
                size_t decoded_len;
                ev->payload = base64_decode(b64buf, b64slen, &decoded_len);
                ev->payload_len = (uint32_t)decoded_len;
            }
            free(b64buf);
        } else if (strcmp(key, "source_offset") == 0) {
            ev->has_source_offset = 1;
            p = json_read_i64(p, end, &ev->source_offset);
        } else if (strcmp(key, "metadata") == 0) {
            /* Parse metadata object */
            if (*p != '{') { p = json_skip_value(p, end); continue; }
            p++;
            /* Count entries first (simplified: allocate reasonable buffer) */
            uint32_t cap = 16;
            ev->metadata_keys   = (char (*)[256])calloc(cap, 256);
            ev->metadata_values = (char (*)[256])calloc(cap, 256);
            ev->metadata_count = 0;

            while (p < end) {
                p = json_skip_ws(p, end);
                if (p >= end) return -1;
                if (*p == '}') { p++; break; }
                if (*p == ',') { p++; continue; }

                if (*p != '"') return -1;
                p++;
                uint32_t idx = ev->metadata_count;
                if (idx >= cap) {
                    cap *= 2;
                    ev->metadata_keys   = (char (*)[256])realloc(ev->metadata_keys, cap * 256);
                    ev->metadata_values = (char (*)[256])realloc(ev->metadata_values, cap * 256);
                }
                p = json_read_string(p, end, ev->metadata_keys[idx], 256);
                if (!p) return -1;

                p = json_skip_ws(p, end);
                if (p >= end || *p != ':') return -1;
                p++;
                p = json_skip_ws(p, end);

                if (*p != '"') return -1;
                p++;
                p = json_read_string(p, end, ev->metadata_values[idx], 256);
                if (!p) return -1;

                ev->metadata_count++;
            }
        } else {
            p = json_skip_value(p, end);
            if (!p) return -1;
        }
    }

    return 0;
}

/* ================================================================== */
/*  JSON encode Output                                                 */
/* ================================================================== */

static uint8_t *json_encode_output(const aeon_output_t *out, size_t *out_len) {
    jbuf_t b;
    jbuf_init(&b);

    jbuf_char(&b, '{');

    /* destination */
    jbuf_str(&b, "\"destination\":");
    jbuf_json_string(&b, out->destination);

    /* payload (base64) */
    jbuf_str(&b, ",\"payload\":\"");
    if (out->payload_len > 0) {
        size_t b64len;
        char *b64 = base64_encode(out->payload, out->payload_len, &b64len);
        if (b64) {
            jbuf_append(&b, b64, b64len);
            free(b64);
        }
    }
    jbuf_char(&b, '"');

    /* key (base64, optional) */
    if (out->key_len > 0 && out->key) {
        jbuf_str(&b, ",\"key\":\"");
        size_t b64len;
        char *b64 = base64_encode(out->key, out->key_len, &b64len);
        if (b64) {
            jbuf_append(&b, b64, b64len);
            free(b64);
        }
        jbuf_char(&b, '"');
    }

    /* headers */
    if (out->header_count > 0) {
        jbuf_str(&b, ",\"headers\":{");
        for (uint32_t i = 0; i < out->header_count; i++) {
            if (i > 0) jbuf_char(&b, ',');
            jbuf_json_string(&b, out->header_keys[i]);
            jbuf_char(&b, ':');
            jbuf_json_string(&b, out->header_values[i]);
        }
        jbuf_char(&b, '}');
    }

    /* source_event_id */
    if (out->source_event_id[0] != '\0') {
        jbuf_str(&b, ",\"source_event_id\":");
        jbuf_json_string(&b, out->source_event_id);
    }

    /* source_partition */
    if (out->has_source_partition) {
        jbuf_str(&b, ",\"source_partition\":");
        {
            char tmp[16];
            snprintf(tmp, sizeof(tmp), "%d", out->source_partition);
            jbuf_str(&b, tmp);
        }
    }

    /* source_offset */
    if (out->has_source_offset) {
        jbuf_str(&b, ",\"source_offset\":");
        jbuf_i64(&b, out->source_offset);
    }

    jbuf_char(&b, '}');

    *out_len = b.len;
    return b.data;
}

/* ================================================================== */
/*  JSON decode Output                                                 */
/* ================================================================== */

static int json_decode_output(const uint8_t *data, size_t len, aeon_output_t *out) {
    memset(out, 0, sizeof(*out));

    const char *p = (const char *)data;
    const char *end = p + len;

    p = json_skip_ws(p, end);
    if (p >= end || *p != '{') return -1;
    p++;

    while (p < end) {
        p = json_skip_ws(p, end);
        if (p >= end) return -1;
        if (*p == '}') break;
        if (*p == ',') { p++; continue; }

        if (*p != '"') return -1;
        p++;
        char key[64];
        p = json_read_string(p, end, key, sizeof(key));
        if (!p) return -1;

        p = json_skip_ws(p, end);
        if (p >= end || *p != ':') return -1;
        p++;
        p = json_skip_ws(p, end);
        if (p >= end) return -1;

        if (strcmp(key, "destination") == 0) {
            if (*p != '"') return -1;
            p++;
            p = json_read_string(p, end, out->destination, sizeof(out->destination));
            if (!p) return -1;
        } else if (strcmp(key, "payload") == 0) {
            if (*p != '"') return -1;
            p++;
            char *b64buf = (char *)malloc(len + 1);
            const char *np = json_read_string(p, end, b64buf, len + 1);
            if (!np) { free(b64buf); return -1; }
            p = np;
            size_t b64slen = strlen(b64buf);
            if (b64slen > 0) {
                size_t decoded_len;
                out->payload = base64_decode(b64buf, b64slen, &decoded_len);
                out->payload_len = (uint32_t)decoded_len;
            }
            free(b64buf);
        } else if (strcmp(key, "key") == 0) {
            if (*p != '"') return -1;
            p++;
            char *b64buf = (char *)malloc(len + 1);
            const char *np = json_read_string(p, end, b64buf, len + 1);
            if (!np) { free(b64buf); return -1; }
            p = np;
            size_t b64slen = strlen(b64buf);
            if (b64slen > 0) {
                size_t decoded_len;
                out->key = base64_decode(b64buf, b64slen, &decoded_len);
                out->key_len = (uint32_t)decoded_len;
            }
            free(b64buf);
        } else if (strcmp(key, "source_event_id") == 0) {
            if (*p != '"') return -1;
            p++;
            p = json_read_string(p, end, out->source_event_id,
                                 sizeof(out->source_event_id));
            if (!p) return -1;
        } else if (strcmp(key, "source_partition") == 0) {
            out->has_source_partition = 1;
            int64_t tmp;
            p = json_read_i64(p, end, &tmp);
            out->source_partition = (int)tmp;
        } else if (strcmp(key, "source_offset") == 0) {
            out->has_source_offset = 1;
            p = json_read_i64(p, end, &out->source_offset);
        } else if (strcmp(key, "headers") == 0) {
            if (*p != '{') { p = json_skip_value(p, end); continue; }
            p++;
            uint32_t cap = 16;
            out->header_keys   = (char (*)[256])calloc(cap, 256);
            out->header_values = (char (*)[256])calloc(cap, 256);
            out->header_count = 0;

            while (p < end) {
                p = json_skip_ws(p, end);
                if (p >= end) return -1;
                if (*p == '}') { p++; break; }
                if (*p == ',') { p++; continue; }

                if (*p != '"') return -1;
                p++;
                uint32_t idx = out->header_count;
                if (idx >= cap) {
                    cap *= 2;
                    out->header_keys   = (char (*)[256])realloc(out->header_keys, cap * 256);
                    out->header_values = (char (*)[256])realloc(out->header_values, cap * 256);
                }
                p = json_read_string(p, end, out->header_keys[idx], 256);
                if (!p) return -1;

                p = json_skip_ws(p, end);
                if (p >= end || *p != ':') return -1;
                p++;
                p = json_skip_ws(p, end);

                if (*p != '"') return -1;
                p++;
                p = json_read_string(p, end, out->header_values[idx], 256);
                if (!p) return -1;

                out->header_count++;
            }
        } else {
            p = json_skip_value(p, end);
            if (!p) return -1;
        }
    }

    return 0;
}

/* ================================================================== */
/*  Wire format: decode batch request                                  */
/* ================================================================== */
/* Format: [8B batch_id LE][4B count LE]                              */
/*         [per event: 4B len + json bytes]                           */
/*         [4B CRC32 LE]                                              */

int aeon_decode_batch_request(const uint8_t *data, size_t len,
                              aeon_batch_request_t *out) {
    if (!data || !out) return -1;
    memset(out, 0, sizeof(*out));

    /* Minimum: 8 + 4 + 4 = 16 bytes (header + CRC, zero events) */
    if (len < 16) return -1;

    /* Verify CRC (covers everything except the last 4 bytes) */
    uint32_t stored_crc = read_u32_le(data + len - 4);
    uint32_t computed_crc = aeon_crc32(data, len - 4);
    if (stored_crc != computed_crc) return -1;

    out->batch_id = read_u64_le(data);
    out->event_count = read_u32_le(data + 8);

    if (out->event_count == 0) {
        out->events = NULL;
        return 0;
    }

    out->events = (aeon_event_t *)calloc(out->event_count, sizeof(aeon_event_t));
    if (!out->events) return -1;

    size_t offset = 12;
    for (uint32_t i = 0; i < out->event_count; i++) {
        if (offset + 4 > len - 4) {
            aeon_free_batch_request(out);
            return -1;
        }
        uint32_t elen = read_u32_le(data + offset);
        offset += 4;
        if (offset + elen > len - 4) {
            aeon_free_batch_request(out);
            return -1;
        }
        if (json_decode_event(data + offset, elen, &out->events[i]) != 0) {
            aeon_free_batch_request(out);
            return -1;
        }
        offset += elen;
    }

    return 0;
}

/* ================================================================== */
/*  Wire format: encode batch response                                 */
/* ================================================================== */

uint8_t *aeon_encode_batch_response(
    uint64_t        batch_id,
    uint32_t        event_count,
    aeon_output_t **outputs_per_event,
    uint32_t       *output_counts,
    size_t         *out_len)
{
    jbuf_t b;
    jbuf_init(&b);

    /* Header: 8B batch_id + 4B event_count */
    jbuf_ensure(&b, 12);
    uint8_t hdr[12];
    write_u64_le(hdr, batch_id);
    write_u32_le(hdr + 8, event_count);
    jbuf_append(&b, (const char *)hdr, 12);

    /* Per event */
    for (uint32_t i = 0; i < event_count; i++) {
        uint32_t oc = output_counts[i];
        uint8_t oc_buf[4];
        write_u32_le(oc_buf, oc);
        jbuf_append(&b, (const char *)oc_buf, 4);

        for (uint32_t j = 0; j < oc; j++) {
            size_t json_len;
            uint8_t *json = json_encode_output(&outputs_per_event[i][j], &json_len);
            uint8_t len_buf[4];
            write_u32_le(len_buf, (uint32_t)json_len);
            jbuf_append(&b, (const char *)len_buf, 4);
            jbuf_append(&b, (const char *)json, json_len);
            free(json);
        }
    }

    /* CRC32 */
    uint32_t crc = aeon_crc32(b.data, b.len);
    uint8_t crc_buf[4];
    write_u32_le(crc_buf, crc);
    jbuf_append(&b, (const char *)crc_buf, 4);

    /* 64-byte zero signature */
    jbuf_ensure(&b, 64);
    memset(b.data + b.len, 0, 64);
    b.len += 64;

    *out_len = b.len;
    return b.data;
}

/* ================================================================== */
/*  Wire format: data frame                                            */
/* ================================================================== */

uint8_t *aeon_build_data_frame(const char *pipeline_name,
                               uint16_t    partition,
                               const uint8_t *batch_data,
                               size_t      batch_data_len,
                               size_t     *out_len)
{
    uint32_t name_len = (uint32_t)strlen(pipeline_name);
    size_t total = 4 + name_len + 2 + batch_data_len;
    uint8_t *buf = (uint8_t *)malloc(total);
    if (!buf) return NULL;

    write_u32_le(buf, name_len);
    memcpy(buf + 4, pipeline_name, name_len);
    write_u16_le(buf + 4 + name_len, partition);
    if (batch_data_len > 0) {
        memcpy(buf + 4 + name_len + 2, batch_data, batch_data_len);
    }

    *out_len = total;
    return buf;
}

int aeon_parse_data_frame(const uint8_t *data, size_t len,
                          aeon_data_frame_t *out) {
    if (!data || !out) return -1;
    memset(out, 0, sizeof(*out));

    /* Minimum: 4 (name_len) + 0 (name) + 2 (partition) = 6 */
    if (len < 6) return -1;

    uint32_t name_len = read_u32_le(data);
    if (4 + name_len + 2 > len) return -1;
    if (name_len >= sizeof(out->pipeline_name)) return -1;

    memcpy(out->pipeline_name, data + 4, name_len);
    out->pipeline_name[name_len] = '\0';

    out->partition = read_u16_le(data + 4 + name_len);

    size_t hdr_size = 4 + name_len + 2;
    out->batch_data = (uint8_t *)(data + hdr_size);
    out->batch_data_len = (uint32_t)(len - hdr_size);

    return 0;
}

/* ================================================================== */
/*  Free functions                                                     */
/* ================================================================== */

void aeon_free_batch_request(aeon_batch_request_t *req) {
    if (!req) return;
    if (req->events) {
        for (uint32_t i = 0; i < req->event_count; i++) {
            free(req->events[i].metadata_keys);
            free(req->events[i].metadata_values);
            free(req->events[i].payload);
        }
        free(req->events);
        req->events = NULL;
    }
    req->event_count = 0;
}

void aeon_free(void *ptr) {
    free(ptr);
}
