// Aeon Processor SDK — Core types.

namespace Aeon.ProcessorSdk;

/// <summary>Inbound event from the Aeon engine.</summary>
public sealed class Event
{
    public string Id { get; set; } = "";
    public long Timestamp { get; set; }
    public string Source { get; set; } = "";
    public int Partition { get; set; }
    public List<string[]> Metadata { get; set; } = new();
    public byte[] Payload { get; set; } = Array.Empty<byte>();
    public long? SourceOffset { get; set; }

    /// <summary>Interpret payload as UTF-8 string.</summary>
    public string PayloadString => System.Text.Encoding.UTF8.GetString(Payload);
}

/// <summary>Outbound record to be written to a sink.</summary>
public sealed class Output
{
    public string Destination { get; set; } = "";
    public byte[] Payload { get; set; } = Array.Empty<byte>();
    public byte[]? Key { get; set; }
    public List<string[]> Headers { get; set; } = new();
    public string? SourceEventId { get; set; }
    public int? SourcePartition { get; set; }
    public long? SourceOffset { get; set; }

    /// <summary>Propagate event identity to this output.</summary>
    public Output WithEventIdentity(Event e)
    {
        SourceEventId = e.Id;
        SourcePartition = e.Partition;
        SourceOffset = e.SourceOffset;
        return this;
    }
}

/// <summary>Per-event processor: one event in, zero or more outputs out.</summary>
public delegate List<Output> ProcessorFunc(Event e);

/// <summary>Batch processor: many events in, one output list per event.</summary>
public delegate List<List<Output>> BatchProcessorFunc(List<Event> events);

/// <summary>Processor registration container.</summary>
public sealed class ProcessorRegistration
{
    public ProcessorFunc? ProcessFn { get; }
    public BatchProcessorFunc? BatchProcessFn { get; }

    private ProcessorRegistration(ProcessorFunc? processFn, BatchProcessorFunc? batchProcessFn)
    {
        ProcessFn = processFn;
        BatchProcessFn = batchProcessFn;
    }

    /// <summary>Register a per-event processor function.</summary>
    public static ProcessorRegistration PerEvent(ProcessorFunc fn) => new(fn, null);

    /// <summary>Register a batch processor function.</summary>
    public static ProcessorRegistration Batch(BatchProcessorFunc fn) => new(null, fn);
}
