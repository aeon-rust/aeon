package io.aeon.processor;

import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Base64;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * Minimal JSON codec for AWPP wire format.
 * Encodes/decodes Event and Output to/from JSON with base64 binary fields.
 * Zero external dependencies — hand-rolled JSON parser.
 */
public final class Codec {

    // ── Encode ──────────────────────────────────────────────────────────

    public byte[] encodeEvent(Event e) {
        var sb = new StringBuilder(256);
        sb.append('{');
        appendString(sb, "id", e.id()); sb.append(',');
        appendNumber(sb, "timestamp", e.timestamp()); sb.append(',');
        appendString(sb, "source", e.source()); sb.append(',');
        appendNumber(sb, "partition", e.partition()); sb.append(',');
        sb.append("\"metadata\":");
        appendStringPairArray(sb, e.metadata());
        sb.append(',');
        sb.append("\"payload\":\"");
        sb.append(Base64.getEncoder().encodeToString(e.payload()));
        sb.append('"');
        if (e.sourceOffset() != null) {
            sb.append(',');
            appendNumber(sb, "source_offset", e.sourceOffset());
        }
        sb.append('}');
        return sb.toString().getBytes(StandardCharsets.UTF_8);
    }

    public Event decodeEvent(byte[] data) {
        var map = parseObject(new String(data, StandardCharsets.UTF_8));
        String id = (String) map.get("id");
        long timestamp = toLong(map.get("timestamp"));
        String source = (String) map.get("source");
        int partition = (int) toLong(map.get("partition"));
        byte[] payload = decodePayload(map.get("payload"));
        @SuppressWarnings("unchecked")
        List<String[]> metadata = decodeMetadata(map.get("metadata"));
        Long sourceOffset = map.containsKey("source_offset") && map.get("source_offset") != null
                ? toLong(map.get("source_offset")) : null;
        return new Event(id, timestamp, source, partition, metadata, payload, sourceOffset);
    }

    public byte[] encodeOutput(Output o) {
        var sb = new StringBuilder(256);
        sb.append('{');
        appendString(sb, "destination", o.destination()); sb.append(',');
        sb.append("\"payload\":");
        appendByteArray(sb, o.payload());
        if (o.key() != null) {
            sb.append(',');
            sb.append("\"key\":");
            appendByteArray(sb, o.key());
        }
        sb.append(',');
        sb.append("\"headers\":");
        appendStringPairArray(sb, o.headers());
        if (o.sourceEventId() != null) {
            sb.append(',');
            appendString(sb, "source_event_id", o.sourceEventId());
        }
        if (o.sourcePartition() != null) {
            sb.append(',');
            appendNumber(sb, "source_partition", o.sourcePartition());
        }
        if (o.sourceOffset() != null) {
            sb.append(',');
            appendNumber(sb, "source_offset", o.sourceOffset());
        }
        sb.append('}');
        return sb.toString().getBytes(StandardCharsets.UTF_8);
    }

    public Output decodeOutput(byte[] data) {
        var map = parseObject(new String(data, StandardCharsets.UTF_8));
        String destination = (String) map.get("destination");
        byte[] payload = decodePayload(map.get("payload"));
        byte[] key = map.containsKey("key") && map.get("key") != null
                ? decodePayload(map.get("key")) : null;
        List<String[]> headers = decodeMetadata(map.get("headers"));
        String sourceEventId = (String) map.get("source_event_id");
        Integer sourcePartition = map.containsKey("source_partition") && map.get("source_partition") != null
                ? (int) toLong(map.get("source_partition")) : null;
        Long sourceOffset = map.containsKey("source_offset") && map.get("source_offset") != null
                ? toLong(map.get("source_offset")) : null;
        return new Output(destination, payload, key, headers, sourceEventId, sourcePartition, sourceOffset);
    }

    // ── Payload decoding ────────────────────────────────────────────────

    /**
     * Decode a payload value which may be either a base64 string (SDK encoding)
     * or a JSON array of byte values (engine's serde encoding for Bytes/Vec<u8>).
     */
    @SuppressWarnings("unchecked")
    private static byte[] decodePayload(Object raw) {
        if (raw == null) return new byte[0];
        if (raw instanceof String s) {
            return Base64.getDecoder().decode(s);
        }
        if (raw instanceof List<?> list) {
            byte[] result = new byte[list.size()];
            for (int i = 0; i < list.size(); i++) {
                result[i] = (byte) ((Number) list.get(i)).intValue();
            }
            return result;
        }
        throw new RuntimeException("Cannot decode payload: unexpected type " + raw.getClass());
    }

    // ── JSON helpers ────────────────────────────────────────────────────

    private static void appendString(StringBuilder sb, String key, String value) {
        sb.append('"').append(escapeJson(key)).append("\":\"").append(escapeJson(value)).append('"');
    }

    private static void appendNumber(StringBuilder sb, String key, long value) {
        sb.append('"').append(escapeJson(key)).append("\":").append(value);
    }

    private static void appendByteArray(StringBuilder sb, byte[] data) {
        sb.append('[');
        for (int i = 0; i < data.length; i++) {
            if (i > 0) sb.append(',');
            sb.append(data[i] & 0xFF);
        }
        sb.append(']');
    }

    private static void appendStringPairArray(StringBuilder sb, List<String[]> pairs) {
        sb.append('[');
        if (pairs != null) {
            for (int i = 0; i < pairs.size(); i++) {
                if (i > 0) sb.append(',');
                String[] pair = pairs.get(i);
                sb.append("[\"").append(escapeJson(pair[0])).append("\",\"").append(escapeJson(pair[1])).append("\"]");
            }
        }
        sb.append(']');
    }

    private static String escapeJson(String s) {
        if (s == null) return "";
        var sb = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            switch (c) {
                case '"' -> sb.append("\\\"");
                case '\\' -> sb.append("\\\\");
                case '\n' -> sb.append("\\n");
                case '\r' -> sb.append("\\r");
                case '\t' -> sb.append("\\t");
                default -> sb.append(c);
            }
        }
        return sb.toString();
    }

    // ── Minimal JSON parser ─────────────────────────────────────────────

    private static long toLong(Object v) {
        if (v instanceof Number n) return n.longValue();
        return Long.parseLong(v.toString().trim());
    }

    @SuppressWarnings("unchecked")
    private static List<String[]> decodeMetadata(Object raw) {
        if (raw == null) return List.of();
        if (raw instanceof List<?> list) {
            var result = new ArrayList<String[]>();
            for (var item : list) {
                if (item instanceof List<?> pair) {
                    result.add(new String[]{pair.get(0).toString(), pair.get(1).toString()});
                }
            }
            return result;
        }
        return List.of();
    }

    /**
     * Simple recursive-descent JSON object parser.
     * Handles: objects, arrays, strings, numbers, null, true, false.
     */
    static Map<String, Object> parseObject(String json) {
        var p = new JsonParser(json.trim());
        Object result = p.parseValue();
        if (result instanceof Map) {
            @SuppressWarnings("unchecked")
            Map<String, Object> map = (Map<String, Object>) result;
            return map;
        }
        throw new RuntimeException("Expected JSON object, got: " + result);
    }

    static final class JsonParser {
        private final String s;
        private int pos;

        JsonParser(String s) {
            this.s = s;
            this.pos = 0;
        }

        Object parseValue() {
            skipWhitespace();
            if (pos >= s.length()) throw new RuntimeException("Unexpected end of JSON");
            char c = s.charAt(pos);
            return switch (c) {
                case '{' -> parseObj();
                case '[' -> parseArr();
                case '"' -> parseStr();
                case 'n' -> { expect("null"); yield null; }
                case 't' -> { expect("true"); yield Boolean.TRUE; }
                case 'f' -> { expect("false"); yield Boolean.FALSE; }
                default -> parseNum();
            };
        }

        private Map<String, Object> parseObj() {
            var map = new LinkedHashMap<String, Object>();
            pos++; // skip {
            skipWhitespace();
            if (pos < s.length() && s.charAt(pos) == '}') { pos++; return map; }
            while (true) {
                skipWhitespace();
                String key = parseStr();
                skipWhitespace();
                if (s.charAt(pos) != ':') throw new RuntimeException("Expected ':' at " + pos);
                pos++;
                Object value = parseValue();
                map.put(key, value);
                skipWhitespace();
                if (pos >= s.length()) break;
                if (s.charAt(pos) == ',') { pos++; continue; }
                if (s.charAt(pos) == '}') { pos++; break; }
                throw new RuntimeException("Expected ',' or '}' at " + pos);
            }
            return map;
        }

        private List<Object> parseArr() {
            var list = new ArrayList<>();
            pos++; // skip [
            skipWhitespace();
            if (pos < s.length() && s.charAt(pos) == ']') { pos++; return list; }
            while (true) {
                list.add(parseValue());
                skipWhitespace();
                if (pos >= s.length()) break;
                if (s.charAt(pos) == ',') { pos++; continue; }
                if (s.charAt(pos) == ']') { pos++; break; }
                throw new RuntimeException("Expected ',' or ']' at " + pos);
            }
            return list;
        }

        private String parseStr() {
            if (s.charAt(pos) != '"') throw new RuntimeException("Expected '\"' at " + pos);
            pos++;
            var sb = new StringBuilder();
            while (pos < s.length()) {
                char c = s.charAt(pos);
                if (c == '"') { pos++; return sb.toString(); }
                if (c == '\\') {
                    pos++;
                    char esc = s.charAt(pos);
                    switch (esc) {
                        case '"' -> sb.append('"');
                        case '\\' -> sb.append('\\');
                        case '/' -> sb.append('/');
                        case 'n' -> sb.append('\n');
                        case 'r' -> sb.append('\r');
                        case 't' -> sb.append('\t');
                        case 'u' -> {
                            String hex = s.substring(pos + 1, pos + 5);
                            sb.append((char) Integer.parseInt(hex, 16));
                            pos += 4;
                        }
                        default -> sb.append(esc);
                    }
                } else {
                    sb.append(c);
                }
                pos++;
            }
            throw new RuntimeException("Unterminated string");
        }

        private Number parseNum() {
            int start = pos;
            if (pos < s.length() && s.charAt(pos) == '-') pos++;
            while (pos < s.length() && Character.isDigit(s.charAt(pos))) pos++;
            boolean isFloat = false;
            if (pos < s.length() && s.charAt(pos) == '.') { isFloat = true; pos++; while (pos < s.length() && Character.isDigit(s.charAt(pos))) pos++; }
            if (pos < s.length() && (s.charAt(pos) == 'e' || s.charAt(pos) == 'E')) { isFloat = true; pos++; if (pos < s.length() && (s.charAt(pos) == '+' || s.charAt(pos) == '-')) pos++; while (pos < s.length() && Character.isDigit(s.charAt(pos))) pos++; }
            String num = s.substring(start, pos);
            if (isFloat) return Double.parseDouble(num);
            long l = Long.parseLong(num);
            return l;
        }

        private void expect(String token) {
            if (!s.startsWith(token, pos))
                throw new RuntimeException("Expected '" + token + "' at " + pos);
            pos += token.length();
        }

        private void skipWhitespace() {
            while (pos < s.length() && Character.isWhitespace(s.charAt(pos))) pos++;
        }
    }
}
