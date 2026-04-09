package io.aeon.processor;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HexFormat;
import java.util.List;

/**
 * Aeon Java Processor SDK test suite.
 * No JUnit — uses a main method with assertions.
 */
public class AeonTest {

    private static int passed = 0;
    private static int failed = 0;
    private static final HexFormat HEX = HexFormat.of();

    public static void main(String[] args) {
        System.out.println("=== Aeon Java Processor SDK Tests ===\n");

        // CRC32 tests
        testCrc32Empty();
        testCrc32KnownValue();
        testCrc32Binary();

        // Signer tests
        testSignerGenerate();
        testSignerFromSeedDeterministic();
        testSignerDifferentSeeds();
        testSignerSignChallenge128Hex();
        testSignerSignAndVerifyBatch();
        testSignerVerifyFailsWrongData();
        testSignerSeedLengthValidation();

        // Codec tests
        testCodecRoundtripEvent();
        testCodecEventWithoutOptionals();
        testCodecRoundtripOutput();
        testCodecOutputWithoutKey();
        testCodecBinaryPayload();

        // DataFrame tests
        testDataFrameRoundtrip();
        testDataFrameEmptyName();
        testDataFrameUnicodeName();
        testDataFrameTooShortThrows();

        // BatchWire tests
        testBatchSignedResponse();
        testBatchUnsignedResponse();
        testBatchRoundtripRequest();
        testBatchCrcMismatch();
        testBatchMultipleEvents();

        // Processor tests
        testProcessorPerEvent();
        testProcessorEventIdentity();
        testProcessorCorrectOutputs();

        // Cross-codec test
        testCrossCodecCrcAndSignature();

        System.out.println("\n=== Results: " + passed + " passed, " + failed + " failed ===");
        if (failed > 0) System.exit(1);
    }

    // ── CRC32 Tests ─────────────────────────────────────────────────────

    static void testCrc32Empty() {
        try {
            int crc = Wire.crc32(new byte[0]);
            assertEquals(0, crc, "CRC32 of empty data");
            pass("testCrc32Empty");
        } catch (Throwable t) { fail("testCrc32Empty", t); }
    }

    static void testCrc32KnownValue() {
        try {
            byte[] data = "123456789".getBytes(StandardCharsets.US_ASCII);
            int crc = Wire.crc32(data);
            assertEquals(0xCBF43926, crc, "CRC32 of '123456789'");
            pass("testCrc32KnownValue");
        } catch (Throwable t) { fail("testCrc32KnownValue", t); }
    }

    static void testCrc32Binary() {
        try {
            byte[] data = new byte[]{0x00, (byte) 0xFF, 0x42, (byte) 0xAB};
            int crc = Wire.crc32(data);
            // Just verify it produces a non-zero result and is deterministic
            int crc2 = Wire.crc32(data);
            assertEquals(crc, crc2, "CRC32 deterministic");
            assertTrue(crc != 0, "CRC32 of binary data should be non-zero");
            pass("testCrc32Binary");
        } catch (Throwable t) { fail("testCrc32Binary", t); }
    }

    // ── Signer Tests ────────────────────────────────────────────────────

    static void testSignerGenerate() {
        try {
            var signer = Signer.generate();
            assertNotNull(signer, "generated signer");
            assertEquals(64, signer.publicKeyHex().length(), "public key hex length (32 bytes = 64 hex chars)");
            assertTrue(signer.publicKeyFormatted().startsWith("ed25519:"), "formatted key starts with ed25519:");
            assertEquals(64, signer.fingerprint().length(), "fingerprint is SHA-256 hex (64 chars)");
            pass("testSignerGenerate");
        } catch (Throwable t) { fail("testSignerGenerate", t); }
    }

    static void testSignerFromSeedDeterministic() {
        try {
            byte[] seed = new byte[32];
            for (int i = 0; i < 32; i++) seed[i] = (byte) i;
            var s1 = Signer.fromSeed(seed);
            var s2 = Signer.fromSeed(seed);
            assertEquals(s1.publicKeyHex(), s2.publicKeyHex(), "same seed -> same public key");
            assertEquals(s1.fingerprint(), s2.fingerprint(), "same seed -> same fingerprint");
            pass("testSignerFromSeedDeterministic");
        } catch (Throwable t) { fail("testSignerFromSeedDeterministic", t); }
    }

    static void testSignerDifferentSeeds() {
        try {
            byte[] seed1 = new byte[32];
            byte[] seed2 = new byte[32];
            Arrays.fill(seed2, (byte) 0xFF);
            var s1 = Signer.fromSeed(seed1);
            var s2 = Signer.fromSeed(seed2);
            assertTrue(!s1.publicKeyHex().equals(s2.publicKeyHex()), "different seeds -> different keys");
            pass("testSignerDifferentSeeds");
        } catch (Throwable t) { fail("testSignerDifferentSeeds", t); }
    }

    static void testSignerSignChallenge128Hex() {
        try {
            var signer = Signer.generate();
            // 128 hex chars = 64 bytes nonce
            String nonce = "a".repeat(128);
            String sig = signer.signChallenge(nonce);
            assertEquals(128, sig.length(), "signature hex length (64 bytes = 128 hex chars)");
            // Verify the signature is valid hex
            HEX.parseHex(sig);
            pass("testSignerSignChallenge128Hex");
        } catch (Throwable t) { fail("testSignerSignChallenge128Hex", t); }
    }

    static void testSignerSignAndVerifyBatch() {
        try {
            var signer = Signer.generate();
            byte[] data = "hello aeon".getBytes(StandardCharsets.UTF_8);
            byte[] sig = signer.signBatch(data);
            assertEquals(64, sig.length, "Ed25519 signature is 64 bytes");
            assertTrue(signer.verify(data, sig), "signature should verify");
            pass("testSignerSignAndVerifyBatch");
        } catch (Throwable t) { fail("testSignerSignAndVerifyBatch", t); }
    }

    static void testSignerVerifyFailsWrongData() {
        try {
            var signer = Signer.generate();
            byte[] data = "correct data".getBytes(StandardCharsets.UTF_8);
            byte[] wrongData = "wrong data".getBytes(StandardCharsets.UTF_8);
            byte[] sig = signer.signBatch(data);
            assertTrue(!signer.verify(wrongData, sig), "signature should NOT verify with wrong data");
            pass("testSignerVerifyFailsWrongData");
        } catch (Throwable t) { fail("testSignerVerifyFailsWrongData", t); }
    }

    static void testSignerSeedLengthValidation() {
        try {
            boolean threw = false;
            try {
                Signer.fromSeed(new byte[16]);
            } catch (IllegalArgumentException e) {
                threw = true;
            }
            assertTrue(threw, "fromSeed with 16 bytes should throw IllegalArgumentException");
            pass("testSignerSeedLengthValidation");
        } catch (Throwable t) { fail("testSignerSeedLengthValidation", t); }
    }

    // ── Codec Tests ─────────────────────────────────────────────────────

    static void testCodecRoundtripEvent() {
        try {
            var codec = new Codec();
            var event = new Event(
                "evt-001", 1700000000000L, "test-source", 3,
                List.of(new String[]{"key1", "val1"}, new String[]{"key2", "val2"}),
                "hello world".getBytes(StandardCharsets.UTF_8),
                42L
            );
            byte[] encoded = codec.encodeEvent(event);
            Event decoded = codec.decodeEvent(encoded);
            assertEquals(event.id(), decoded.id(), "event id");
            assertEquals(event.timestamp(), decoded.timestamp(), "event timestamp");
            assertEquals(event.source(), decoded.source(), "event source");
            assertEquals(event.partition(), decoded.partition(), "event partition");
            assertEquals(event.sourceOffset(), decoded.sourceOffset(), "event sourceOffset");
            assertEquals("hello world", decoded.payloadString(), "event payload");
            assertEquals(2, decoded.metadata().size(), "metadata size");
            assertEquals("key1", decoded.metadata().get(0)[0], "metadata[0] key");
            assertEquals("val1", decoded.metadata().get(0)[1], "metadata[0] value");
            pass("testCodecRoundtripEvent");
        } catch (Throwable t) { fail("testCodecRoundtripEvent", t); }
    }

    static void testCodecEventWithoutOptionals() {
        try {
            var codec = new Codec();
            var event = new Event(
                "evt-002", 1700000000000L, "src", 0,
                List.of(), "data".getBytes(StandardCharsets.UTF_8), null
            );
            byte[] encoded = codec.encodeEvent(event);
            Event decoded = codec.decodeEvent(encoded);
            assertEquals(event.id(), decoded.id(), "event id");
            assertNull(decoded.sourceOffset(), "sourceOffset should be null");
            assertEquals(0, decoded.metadata().size(), "empty metadata");
            pass("testCodecEventWithoutOptionals");
        } catch (Throwable t) { fail("testCodecEventWithoutOptionals", t); }
    }

    static void testCodecRoundtripOutput() {
        try {
            var codec = new Codec();
            var output = new Output(
                "sink-1",
                "output data".getBytes(StandardCharsets.UTF_8),
                "my-key".getBytes(StandardCharsets.UTF_8),
                headers(new String[]{"h1", "v1"}),
                "evt-src-001", 5, 100L
            );
            byte[] encoded = codec.encodeOutput(output);
            Output decoded = codec.decodeOutput(encoded);
            assertEquals(output.destination(), decoded.destination(), "output destination");
            assertTrue(Arrays.equals(output.payload(), decoded.payload()), "output payload");
            assertTrue(Arrays.equals(output.key(), decoded.key()), "output key");
            assertEquals(output.sourceEventId(), decoded.sourceEventId(), "output sourceEventId");
            assertEquals(output.sourcePartition(), decoded.sourcePartition(), "output sourcePartition");
            assertEquals(output.sourceOffset(), decoded.sourceOffset(), "output sourceOffset");
            assertEquals(1, decoded.headers().size(), "headers size");
            pass("testCodecRoundtripOutput");
        } catch (Throwable t) { fail("testCodecRoundtripOutput", t); }
    }

    static void testCodecOutputWithoutKey() {
        try {
            var codec = new Codec();
            var output = new Output(
                "sink-2",
                "data".getBytes(StandardCharsets.UTF_8),
                null, List.of(), null, null, null
            );
            byte[] encoded = codec.encodeOutput(output);
            Output decoded = codec.decodeOutput(encoded);
            assertEquals(output.destination(), decoded.destination(), "destination");
            assertNull(decoded.key(), "key should be null");
            assertNull(decoded.sourceEventId(), "sourceEventId should be null");
            assertNull(decoded.sourcePartition(), "sourcePartition should be null");
            assertNull(decoded.sourceOffset(), "sourceOffset should be null");
            pass("testCodecOutputWithoutKey");
        } catch (Throwable t) { fail("testCodecOutputWithoutKey", t); }
    }

    static void testCodecBinaryPayload() {
        try {
            var codec = new Codec();
            byte[] binary = new byte[256];
            for (int i = 0; i < 256; i++) binary[i] = (byte) i;
            var event = new Event("evt-bin", 0L, "s", 0, List.of(), binary, null);
            byte[] encoded = codec.encodeEvent(event);
            Event decoded = codec.decodeEvent(encoded);
            assertTrue(Arrays.equals(binary, decoded.payload()), "binary payload roundtrip");
            pass("testCodecBinaryPayload");
        } catch (Throwable t) { fail("testCodecBinaryPayload", t); }
    }

    // ── DataFrame Tests ─────────────────────────────────────────────────

    static void testDataFrameRoundtrip() {
        try {
            byte[] batchData = "batch-content".getBytes(StandardCharsets.UTF_8);
            byte[] frame = Wire.buildDataFrame("my-pipeline", 7, batchData);
            var parsed = Wire.parseDataFrame(frame);
            assertEquals("my-pipeline", parsed.pipelineName(), "pipeline name");
            assertEquals(7, parsed.partition(), "partition");
            assertTrue(Arrays.equals(batchData, parsed.batchData()), "batch data");
            pass("testDataFrameRoundtrip");
        } catch (Throwable t) { fail("testDataFrameRoundtrip", t); }
    }

    static void testDataFrameEmptyName() {
        try {
            byte[] batchData = "data".getBytes(StandardCharsets.UTF_8);
            byte[] frame = Wire.buildDataFrame("", 0, batchData);
            var parsed = Wire.parseDataFrame(frame);
            assertEquals("", parsed.pipelineName(), "empty pipeline name");
            assertEquals(0, parsed.partition(), "partition");
            assertTrue(Arrays.equals(batchData, parsed.batchData()), "batch data");
            pass("testDataFrameEmptyName");
        } catch (Throwable t) { fail("testDataFrameEmptyName", t); }
    }

    static void testDataFrameUnicodeName() {
        try {
            String name = "pipeline-\u00e9v\u00e9nement-\u2603";
            byte[] batchData = new byte[]{1, 2, 3};
            byte[] frame = Wire.buildDataFrame(name, 42, batchData);
            var parsed = Wire.parseDataFrame(frame);
            assertEquals(name, parsed.pipelineName(), "unicode pipeline name");
            assertEquals(42, parsed.partition(), "partition");
            pass("testDataFrameUnicodeName");
        } catch (Throwable t) { fail("testDataFrameUnicodeName", t); }
    }

    static void testDataFrameTooShortThrows() {
        try {
            boolean threw = false;
            try {
                Wire.parseDataFrame(new byte[]{0x01, 0x02});
            } catch (RuntimeException e) {
                threw = true;
            }
            assertTrue(threw, "parseDataFrame with too-short data should throw");
            pass("testDataFrameTooShortThrows");
        } catch (Throwable t) { fail("testDataFrameTooShortThrows", t); }
    }

    // ── BatchWire Tests ─────────────────────────────────────────────────

    static void testBatchSignedResponse() {
        try {
            var codec = new Codec();
            var signer = Signer.generate();
            var outputs = List.of(List.of(
                new Output("dest", "out".getBytes(StandardCharsets.UTF_8), null, List.of(), null, null, null)
            ));
            byte[] response = Wire.encodeBatchResponse(42L, outputs, codec, signer, true);
            // Should have signature at end: body + 4B CRC + 64B sig
            assertTrue(response.length > 64 + 4, "signed response has signature");

            // Verify the signature
            byte[] toVerify = new byte[response.length - 64];
            System.arraycopy(response, 0, toVerify, 0, toVerify.length);
            byte[] sig = new byte[64];
            System.arraycopy(response, response.length - 64, sig, 0, 64);
            assertTrue(signer.verify(toVerify, sig), "batch signature verifies");
            pass("testBatchSignedResponse");
        } catch (Throwable t) { fail("testBatchSignedResponse", t); }
    }

    static void testBatchUnsignedResponse() {
        try {
            var codec = new Codec();
            var signer = Signer.generate();
            var outputs = List.of(List.of(
                new Output("dest", "out".getBytes(StandardCharsets.UTF_8), null, List.of(), null, null, null)
            ));
            byte[] signedResponse = Wire.encodeBatchResponse(42L, outputs, codec, signer, true);
            byte[] unsignedResponse = Wire.encodeBatchResponse(42L, outputs, codec, signer, false);
            assertEquals(signedResponse.length - 64, unsignedResponse.length, "unsigned = signed - 64 bytes");
            pass("testBatchUnsignedResponse");
        } catch (Throwable t) { fail("testBatchUnsignedResponse", t); }
    }

    static void testBatchRoundtripRequest() {
        try {
            var codec = new Codec();
            // Manually build a batch request
            var event = new Event("evt-rt", 1000L, "src", 1, List.of(), "pay".getBytes(StandardCharsets.UTF_8), null);
            byte[] eventBytes = codec.encodeEvent(event);

            var buf = new java.io.ByteArrayOutputStream();
            // batch_id = 99
            writeBuf(buf, longToLE(99L));
            // count = 1
            writeBuf(buf, intToLE(1));
            // event len + data
            writeBuf(buf, intToLE(eventBytes.length));
            buf.write(eventBytes);

            byte[] body = buf.toByteArray();
            int crc = Wire.crc32(body);
            writeBuf(buf, intToLE(crc)); // This appends to buf which already has body... rebuild
            // Rebuild properly
            var fullBuf = new java.io.ByteArrayOutputStream();
            fullBuf.write(body);
            fullBuf.write(intToLE(crc));
            byte[] requestBytes = fullBuf.toByteArray();

            var parsed = Wire.decodeBatchRequest(requestBytes, codec);
            assertEquals(99L, parsed.batchId(), "batch id");
            assertEquals(1, parsed.events().size(), "event count");
            assertEquals("evt-rt", parsed.events().get(0).id(), "event id");
            pass("testBatchRoundtripRequest");
        } catch (Throwable t) { fail("testBatchRoundtripRequest", t); }
    }

    static void testBatchCrcMismatch() {
        try {
            var codec = new Codec();
            var event = new Event("evt-crc", 1000L, "src", 0, List.of(), "x".getBytes(StandardCharsets.UTF_8), null);
            byte[] eventBytes = codec.encodeEvent(event);

            var buf = new java.io.ByteArrayOutputStream();
            buf.write(longToLE(1L));
            buf.write(intToLE(1));
            buf.write(intToLE(eventBytes.length));
            buf.write(eventBytes);
            byte[] body = buf.toByteArray();
            // Write wrong CRC
            buf.write(intToLE(0xDEADBEEF));
            byte[] requestBytes = buf.toByteArray();

            boolean threw = false;
            try {
                Wire.decodeBatchRequest(requestBytes, codec);
            } catch (RuntimeException e) {
                threw = e.getMessage().contains("CRC32 mismatch");
            }
            assertTrue(threw, "CRC mismatch should throw");
            pass("testBatchCrcMismatch");
        } catch (Throwable t) { fail("testBatchCrcMismatch", t); }
    }

    static void testBatchMultipleEvents() {
        try {
            var codec = new Codec();
            var events = List.of(
                new Event("e1", 100L, "s", 0, List.of(), "a".getBytes(StandardCharsets.UTF_8), null),
                new Event("e2", 200L, "s", 0, List.of(), "b".getBytes(StandardCharsets.UTF_8), null),
                new Event("e3", 300L, "s", 0, List.of(), "c".getBytes(StandardCharsets.UTF_8), null)
            );

            var buf = new java.io.ByteArrayOutputStream();
            buf.write(longToLE(7L));
            buf.write(intToLE(3));
            for (var evt : events) {
                byte[] eb = codec.encodeEvent(evt);
                buf.write(intToLE(eb.length));
                buf.write(eb);
            }
            byte[] body = buf.toByteArray();
            int crc = Wire.crc32(body);
            buf.write(intToLE(crc));
            byte[] requestBytes = buf.toByteArray();

            var parsed = Wire.decodeBatchRequest(requestBytes, codec);
            assertEquals(7L, parsed.batchId(), "batch id");
            assertEquals(3, parsed.events().size(), "3 events");
            assertEquals("e1", parsed.events().get(0).id(), "first event");
            assertEquals("e3", parsed.events().get(2).id(), "third event");
            pass("testBatchMultipleEvents");
        } catch (Throwable t) { fail("testBatchMultipleEvents", t); }
    }

    // ── Processor Tests ─────────────────────────────────────────────────

    static void testProcessorPerEvent() {
        try {
            Processor p = Processor.perEvent(event -> List.of(
                new Output("out", event.payload(), null, List.of(), null, null, null)
            ));
            var event = new Event("e1", 0L, "s", 0, List.of(), "data".getBytes(StandardCharsets.UTF_8), null);
            var outputs = p.process(event);
            assertEquals(1, outputs.size(), "one output");
            assertTrue(Arrays.equals(event.payload(), outputs.get(0).payload()), "payload forwarded");
            pass("testProcessorPerEvent");
        } catch (Throwable t) { fail("testProcessorPerEvent", t); }
    }

    static void testProcessorEventIdentity() {
        try {
            var event = new Event("evt-id-123", 5000L, "src", 7, List.of(), "d".getBytes(StandardCharsets.UTF_8), 99L);
            var output = new Output("dest", "out".getBytes(StandardCharsets.UTF_8), null, headers(), null, null, null);
            var linked = output.withEventIdentity(event);
            assertEquals("evt-id-123", linked.sourceEventId(), "sourceEventId from event");
            assertEquals(Integer.valueOf(7), linked.sourcePartition(), "sourcePartition from event");
            assertEquals(Long.valueOf(99L), linked.sourceOffset(), "sourceOffset from event");
            pass("testProcessorEventIdentity");
        } catch (Throwable t) { fail("testProcessorEventIdentity", t); }
    }

    static void testProcessorCorrectOutputs() {
        try {
            Processor p = Processor.perEvent(event -> {
                var out1 = new Output("d1", "o1".getBytes(StandardCharsets.UTF_8), null, List.of(), null, null, null);
                var out2 = new Output("d2", "o2".getBytes(StandardCharsets.UTF_8), null, List.of(), null, null, null);
                return List.of(out1, out2);
            });
            var event = new Event("e1", 0L, "s", 0, List.of(), new byte[0], null);
            var outputs = p.process(event);
            assertEquals(2, outputs.size(), "two outputs");
            assertEquals("d1", outputs.get(0).destination(), "first destination");
            assertEquals("d2", outputs.get(1).destination(), "second destination");
            pass("testProcessorCorrectOutputs");
        } catch (Throwable t) { fail("testProcessorCorrectOutputs", t); }
    }

    // ── Cross-codec Test ────────────────────────────────────────────────

    static void testCrossCodecCrcAndSignature() {
        try {
            var codec = new Codec();
            var signer = Signer.generate();
            var outputs = List.of(
                List.of(
                    new Output("d1", "p1".getBytes(StandardCharsets.UTF_8), "k1".getBytes(StandardCharsets.UTF_8),
                        headers(new String[]{"h", "v"}), "e1", 0, 10L)
                ),
                List.of(
                    new Output("d2", "p2".getBytes(StandardCharsets.UTF_8), null, List.of(), null, null, null),
                    new Output("d3", "p3".getBytes(StandardCharsets.UTF_8), null, List.of(), null, null, null)
                )
            );

            byte[] response = Wire.encodeBatchResponse(123L, outputs, codec, signer, true);

            // Verify CRC: read it from the correct position (response.length - 64 - 4)
            int crcOffset = response.length - 64 - 4;
            var crcBuf = ByteBuffer.wrap(response, crcOffset, 4).order(ByteOrder.LITTLE_ENDIAN);
            int storedCrc = crcBuf.getInt();
            byte[] bodyForCrc = new byte[crcOffset];
            System.arraycopy(response, 0, bodyForCrc, 0, crcOffset);
            int computedCrc = Wire.crc32(bodyForCrc);
            assertEquals(storedCrc, computedCrc, "CRC matches");

            // Verify signature
            byte[] toVerify = new byte[response.length - 64];
            System.arraycopy(response, 0, toVerify, 0, toVerify.length);
            byte[] sig = new byte[64];
            System.arraycopy(response, response.length - 64, sig, 0, 64);
            assertTrue(signer.verify(toVerify, sig), "cross-codec signature verifies");

            pass("testCrossCodecCrcAndSignature");
        } catch (Throwable t) { fail("testCrossCodecCrcAndSignature", t); }
    }

    // ── Test Helpers ────────────────────────────────────────────────────

    static void assertEquals(Object expected, Object actual, String msg) {
        if (expected == null && actual == null) return;
        if (expected != null && expected.equals(actual)) return;
        throw new AssertionError(msg + ": expected <" + expected + "> but got <" + actual + ">");
    }

    static void assertEquals(long expected, long actual, String msg) {
        if (expected != actual)
            throw new AssertionError(msg + ": expected <" + expected + "> but got <" + actual + ">");
    }

    static void assertEquals(int expected, int actual, String msg) {
        if (expected != actual)
            throw new AssertionError(msg + ": expected <" + expected + "> but got <" + actual + ">");
    }

    static void assertTrue(boolean condition, String msg) {
        if (!condition) throw new AssertionError(msg);
    }

    static void assertNotNull(Object obj, String msg) {
        if (obj == null) throw new AssertionError(msg + " should not be null");
    }

    static void assertNull(Object obj, String msg) {
        if (obj != null) throw new AssertionError(msg + ": expected null but got <" + obj + ">");
    }

    static void pass(String name) {
        System.out.println("[PASS] " + name);
        passed++;
    }

    static void fail(String name, Throwable t) {
        System.out.println("[FAIL] " + name + ": " + t.getMessage());
        failed++;
    }

    @SuppressWarnings("unchecked")
    static <T> List<T> listOf(T... items) {
        return List.of(items);
    }

    static List<String[]> headers(String[]... pairs) {
        return List.of(pairs);
    }

    static byte[] longToLE(long v) {
        return ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN).putLong(v).array();
    }

    static byte[] intToLE(int v) {
        return ByteBuffer.allocate(4).order(ByteOrder.LITTLE_ENDIAN).putInt(v).array();
    }

    static void writeBuf(java.io.ByteArrayOutputStream buf, byte[] data) {
        buf.writeBytes(data);
    }
}
