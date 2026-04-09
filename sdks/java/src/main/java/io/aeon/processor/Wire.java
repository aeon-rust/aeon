package io.aeon.processor;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.zip.CRC32;

/**
 * AWPP wire format encoder/decoder.
 * All integers are LITTLE-ENDIAN. CRC32 uses IEEE polynomial.
 */
public final class Wire {

    private Wire() {}

    // ── CRC32 ───────────────────────────────────────────────────────────

    /**
     * Compute CRC32 (IEEE) of data, return as unsigned 32-bit int.
     */
    public static int crc32(byte[] data) {
        var crc = new CRC32();
        crc.update(data);
        return (int) crc.getValue();
    }

    // ── Batch Request ───────────────────────────────────────────────────

    /**
     * Decoded batch request from wire format.
     */
    public record BatchRequest(long batchId, List<Event> events) {}

    /**
     * Decode a batch request from wire bytes.
     * Format: [8B batch_id LE][4B count LE][per event: 4B len LE + codec bytes][4B CRC32 LE]
     */
    public static BatchRequest decodeBatchRequest(byte[] data, Codec codec) {
        var buf = ByteBuffer.wrap(data).order(ByteOrder.LITTLE_ENDIAN);

        long batchId = buf.getLong();       // 8 bytes
        int count = buf.getInt();           // 4 bytes

        // Compute CRC over everything except last 4 bytes
        int crcOffset = data.length - 4;
        var crcBuf = ByteBuffer.wrap(data, crcOffset, 4).order(ByteOrder.LITTLE_ENDIAN);
        int expectedCrc = crcBuf.getInt();

        byte[] crcData = new byte[crcOffset];
        System.arraycopy(data, 0, crcData, 0, crcOffset);
        int actualCrc = crc32(crcData);

        if (actualCrc != expectedCrc) {
            throw new RuntimeException(
                "CRC32 mismatch: expected " + Integer.toUnsignedString(expectedCrc) +
                ", got " + Integer.toUnsignedString(actualCrc)
            );
        }

        var events = new ArrayList<Event>(count);
        for (int i = 0; i < count; i++) {
            int len = buf.getInt();
            byte[] eventBytes = new byte[len];
            buf.get(eventBytes);
            events.add(codec.decodeEvent(eventBytes));
        }

        return new BatchRequest(batchId, events);
    }

    // ── Batch Response ──────────────────────────────────────────────────

    /**
     * Encode a batch response to wire bytes.
     * Format: [8B batch_id LE][4B count LE][per event: 4B output_count LE [per output: 4B len LE + codec bytes]][4B CRC32 LE][64B ED25519 sig]
     */
    public static byte[] encodeBatchResponse(
            long batchId,
            List<List<Output>> outputsPerEvent,
            Codec codec,
            Signer signer,
            boolean batchSigning
    ) {
        // Build body first to calculate size
        var bodyBuf = new java.io.ByteArrayOutputStream(4096);

        // batch_id (8 bytes)
        writeLE8(bodyBuf, batchId);

        // count (4 bytes)
        int count = outputsPerEvent.size();
        writeLE4(bodyBuf, count);

        // Per-event outputs
        for (var outputs : outputsPerEvent) {
            writeLE4(bodyBuf, outputs.size());
            for (var output : outputs) {
                byte[] encoded = codec.encodeOutput(output);
                writeLE4(bodyBuf, encoded.length);
                bodyBuf.writeBytes(encoded);
            }
        }

        byte[] body = bodyBuf.toByteArray();

        // CRC32 of body
        int crc = crc32(body);

        // Build final buffer: body + CRC + optional signature
        int totalLen = body.length + 4 + (batchSigning ? 64 : 0);
        var result = ByteBuffer.allocate(totalLen).order(ByteOrder.LITTLE_ENDIAN);
        result.put(body);
        result.putInt(crc);

        if (batchSigning) {
            // Sign: body + CRC
            byte[] toSign = new byte[body.length + 4];
            System.arraycopy(body, 0, toSign, 0, body.length);
            ByteBuffer.wrap(toSign, body.length, 4).order(ByteOrder.LITTLE_ENDIAN).putInt(crc);
            byte[] signature = signer.signBatch(toSign);
            result.put(signature);
        }

        return result.array();
    }

    // ── Data Frame ──────────────────────────────────────────────────────

    /**
     * Decoded data frame with routing header.
     */
    public record DataFrame(String pipelineName, int partition, byte[] batchData) {}

    /**
     * Build a T4 data frame with routing header.
     * Format: [4B name_len LE][pipeline_name UTF-8][2B partition LE][batch_data]
     */
    public static byte[] buildDataFrame(String pipelineName, int partition, byte[] batchData) {
        byte[] nameBytes = pipelineName.getBytes(StandardCharsets.UTF_8);
        var buf = ByteBuffer.allocate(4 + nameBytes.length + 2 + batchData.length)
                .order(ByteOrder.LITTLE_ENDIAN);
        buf.putInt(nameBytes.length);
        buf.put(nameBytes);
        buf.putShort((short) partition);
        buf.put(batchData);
        return buf.array();
    }

    /**
     * Parse a T4 data frame from wire bytes.
     */
    public static DataFrame parseDataFrame(byte[] data) {
        if (data.length < 6) {
            throw new RuntimeException("Data frame too short: " + data.length + " bytes (minimum 6)");
        }
        var buf = ByteBuffer.wrap(data).order(ByteOrder.LITTLE_ENDIAN);
        int nameLen = buf.getInt();
        if (data.length < 4 + nameLen + 2) {
            throw new RuntimeException("Data frame too short for name length " + nameLen);
        }
        byte[] nameBytes = new byte[nameLen];
        buf.get(nameBytes);
        String pipelineName = new String(nameBytes, StandardCharsets.UTF_8);
        int partition = Short.toUnsignedInt(buf.getShort());
        byte[] batchData = new byte[buf.remaining()];
        buf.get(batchData);
        return new DataFrame(pipelineName, partition, batchData);
    }

    // ── LE write helpers ────────────────────────────────────────────────

    private static void writeLE8(java.io.ByteArrayOutputStream out, long value) {
        var buf = ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN);
        buf.putLong(value);
        out.writeBytes(buf.array());
    }

    private static void writeLE4(java.io.ByteArrayOutputStream out, int value) {
        var buf = ByteBuffer.allocate(4).order(ByteOrder.LITTLE_ENDIAN);
        buf.putInt(value);
        out.writeBytes(buf.array());
    }
}
