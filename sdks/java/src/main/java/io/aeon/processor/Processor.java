package io.aeon.processor;

import java.util.List;
import java.util.function.Function;

/**
 * Per-event processor interface for AWPP.
 */
@FunctionalInterface
public interface Processor {

    /**
     * Process a single event, producing zero or more outputs.
     */
    List<Output> process(Event event);

    /**
     * Create a Processor from a per-event function.
     */
    static Processor perEvent(Function<Event, List<Output>> fn) {
        return fn::apply;
    }

    /**
     * Create a BatchProcessor from a batch function.
     */
    static BatchProcessor batch(Function<List<Event>, List<List<Output>>> fn) {
        return fn::apply;
    }
}
