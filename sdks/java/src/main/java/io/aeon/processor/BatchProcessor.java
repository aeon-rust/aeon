package io.aeon.processor;

import java.util.List;

/**
 * Batch processor interface for AWPP.
 * Processes a list of events and returns a list of output lists (one per input event).
 */
@FunctionalInterface
public interface BatchProcessor {

    /**
     * Process a batch of events. Returns a list of output lists,
     * where outputsPerEvent.get(i) corresponds to events.get(i).
     */
    List<List<Output>> processBatch(List<Event> events);
}
