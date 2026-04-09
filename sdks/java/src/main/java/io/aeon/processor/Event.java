package io.aeon.processor;

import java.nio.charset.StandardCharsets;
import java.util.List;

/**
 * Canonical Aeon event envelope for the Java Processor SDK.
 */
public record Event(
    String id,
    long timestamp,
    String source,
    int partition,
    List<String[]> metadata,
    byte[] payload,
    Long sourceOffset
) {
    /**
     * Return payload decoded as UTF-8 string.
     */
    public String payloadString() {
        return new String(payload, StandardCharsets.UTF_8);
    }
}
