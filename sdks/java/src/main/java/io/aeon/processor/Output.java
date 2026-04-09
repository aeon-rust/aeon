package io.aeon.processor;

import java.util.List;

/**
 * Output produced by a processor, destined for an Aeon sink.
 */
public record Output(
    String destination,
    byte[] payload,
    byte[] key,
    List<String[]> headers,
    String sourceEventId,
    Integer sourcePartition,
    Long sourceOffset
) {
    /**
     * Create a new Output carrying the identity (id, partition, offset) of the source event.
     */
    public Output withEventIdentity(Event e) {
        return new Output(
            destination, payload, key, headers,
            e.id(), e.partition(), e.sourceOffset()
        );
    }
}
