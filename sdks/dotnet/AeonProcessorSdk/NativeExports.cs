// Aeon Processor SDK — T1 NativeAOT C-ABI exports.
//
// When compiled with `dotnet publish -c Release -r <rid> /p:PublishAot=true`,
// this produces a native shared library (.dll/.so/.dylib) with the standard
// Aeon processor C-ABI:
//
//   aeon_processor_create(config_ptr, config_len) -> void*
//   aeon_processor_destroy(ctx)
//   aeon_process(ctx, event_ptr, event_len, out_buf, out_capacity, out_len) -> int32
//   aeon_process_batch(ctx, events_ptr, events_len, out_buf, out_capacity, out_len) -> int32
//   aeon_processor_name() -> const char*
//   aeon_processor_version() -> const char*
//
// Usage:
//   1. Set NativeExportConfig.ProcessorFactory before any calls.
//   2. Compile with NativeAOT.
//   3. The engine loads the resulting .dll/.so and calls the C-ABI functions.

using System.Buffers.Binary;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using System.Text;

namespace Aeon.ProcessorSdk;

/// <summary>
/// Interface for T1 native processors.
/// Implement this to create a processor loadable via C-ABI.
/// </summary>
public interface INativeProcessor
{
    /// <summary>Process a single event, returning serialized outputs.</summary>
    byte[] Process(ReadOnlySpan<byte> eventData);

    /// <summary>Process a batch of events, returning serialized outputs.</summary>
    byte[] ProcessBatch(ReadOnlySpan<byte> batchData);
}

/// <summary>
/// Configuration for NativeAOT exports.
/// Set ProcessorFactory before the library is loaded by the engine.
/// </summary>
public static class NativeExportConfig
{
    /// <summary>Factory to create processor instances from config bytes.</summary>
    public static Func<byte[], INativeProcessor>? ProcessorFactory { get; set; }

    /// <summary>Processor name returned by aeon_processor_name().</summary>
    public static string ProcessorName { get; set; } = "dotnet-processor";

    /// <summary>Processor version returned by aeon_processor_version().</summary>
    public static string ProcessorVersion { get; set; } = "1.0.0";
}

/// <summary>
/// NativeAOT C-ABI exports matching the Aeon native processor interface.
/// These functions are exported with [UnmanagedCallersOnly] so the Aeon engine
/// can load them via dlopen/LoadLibrary.
/// </summary>
public static class NativeExports
{
    // GCHandle storage for processor instances
    private static readonly Dictionary<nint, (GCHandle Handle, INativeProcessor Proc)> _instances = new();
    private static nint _nextHandle = 1;
    private static readonly object _lock = new();

    private static nint RegisterInstance(INativeProcessor proc)
    {
        var gcHandle = GCHandle.Alloc(proc);
        lock (_lock)
        {
            var id = _nextHandle++;
            _instances[id] = (gcHandle, proc);
            return id;
        }
    }

    /// <summary>Create a processor instance from configuration bytes.</summary>
    [UnmanagedCallersOnly(EntryPoint = "aeon_processor_create")]
    public static unsafe nint AeonProcessorCreate(byte* configPtr, nuint configLen)
    {
        try
        {
            if (NativeExportConfig.ProcessorFactory == null)
                return 0;

            byte[] config;
            if (configPtr != null && configLen > 0)
            {
                config = new byte[(int)configLen];
                new ReadOnlySpan<byte>(configPtr, (int)configLen).CopyTo(config);
            }
            else
            {
                config = Array.Empty<byte>();
            }

            var proc = NativeExportConfig.ProcessorFactory(config);
            return RegisterInstance(proc);
        }
        catch
        {
            return 0; // NULL = failure
        }
    }

    /// <summary>Destroy a processor instance.</summary>
    [UnmanagedCallersOnly(EntryPoint = "aeon_processor_destroy")]
    public static void AeonProcessorDestroy(nint ctx)
    {
        lock (_lock)
        {
            if (_instances.TryGetValue(ctx, out var entry))
            {
                entry.Handle.Free();
                _instances.Remove(ctx);
            }
        }
    }

    /// <summary>Process a single event.</summary>
    /// <returns>0=success, -1=invalid params, -2=deserialize error, -3=processing error, -4=buffer too small</returns>
    [UnmanagedCallersOnly(EntryPoint = "aeon_process")]
    public static unsafe int AeonProcess(
        nint ctx,
        byte* eventPtr, nuint eventLen,
        byte* outBuf, nuint outCapacity,
        nuint* outLen)
    {
        if (eventPtr == null || outBuf == null || outLen == null)
            return -1;

        INativeProcessor? proc;
        lock (_lock)
        {
            if (!_instances.TryGetValue(ctx, out var entry))
                return -1;
            proc = entry.Proc;
        }

        try
        {
            var eventData = new ReadOnlySpan<byte>(eventPtr, (int)eventLen);
            var result = proc.Process(eventData);

            if ((nuint)result.Length > outCapacity)
            {
                *outLen = (nuint)result.Length;
                return -4; // buffer too small
            }

            result.CopyTo(new Span<byte>(outBuf, (int)outCapacity));
            *outLen = (nuint)result.Length;
            return 0;
        }
        catch
        {
            return -3;
        }
    }

    /// <summary>Process a batch of events.</summary>
    [UnmanagedCallersOnly(EntryPoint = "aeon_process_batch")]
    public static unsafe int AeonProcessBatch(
        nint ctx,
        byte* eventsPtr, nuint eventsLen,
        byte* outBuf, nuint outCapacity,
        nuint* outLen)
    {
        if (eventsPtr == null || outBuf == null || outLen == null)
            return -1;

        INativeProcessor? proc;
        lock (_lock)
        {
            if (!_instances.TryGetValue(ctx, out var entry))
                return -1;
            proc = entry.Proc;
        }

        try
        {
            var batchData = new ReadOnlySpan<byte>(eventsPtr, (int)eventsLen);
            var result = proc.ProcessBatch(batchData);

            if ((nuint)result.Length > outCapacity)
            {
                *outLen = (nuint)result.Length;
                return -4;
            }

            result.CopyTo(new Span<byte>(outBuf, (int)outCapacity));
            *outLen = (nuint)result.Length;
            return 0;
        }
        catch
        {
            return -3;
        }
    }

    // Static byte arrays for name/version (must stay pinned for C interop)
    private static byte[]? _nameBytes;
    private static byte[]? _versionBytes;
    private static GCHandle _namePinned;
    private static GCHandle _versionPinned;

    /// <summary>Return the processor name as a null-terminated C string.</summary>
    [UnmanagedCallersOnly(EntryPoint = "aeon_processor_name")]
    public static unsafe byte* AeonProcessorName()
    {
        if (_nameBytes == null)
        {
            _nameBytes = Encoding.UTF8.GetBytes(NativeExportConfig.ProcessorName + "\0");
            _namePinned = GCHandle.Alloc(_nameBytes, GCHandleType.Pinned);
        }
        return (byte*)_namePinned.AddrOfPinnedObject();
    }

    /// <summary>Return the processor version as a null-terminated C string.</summary>
    [UnmanagedCallersOnly(EntryPoint = "aeon_processor_version")]
    public static unsafe byte* AeonProcessorVersion()
    {
        if (_versionBytes == null)
        {
            _versionBytes = Encoding.UTF8.GetBytes(NativeExportConfig.ProcessorVersion + "\0");
            _versionPinned = GCHandle.Alloc(_versionBytes, GCHandleType.Pinned);
        }
        return (byte*)_versionPinned.AddrOfPinnedObject();
    }
}

/// <summary>
/// Native wire format helpers for T1 processors.
/// Matches the binary event/output format used by aeon-native-sdk.
/// </summary>
public static class NativeWire
{
    /// <summary>Deserialize an event from native wire format.</summary>
    public static Event DeserializeEvent(ReadOnlySpan<byte> data)
    {
        int offset = 0;

        // UUID (16 bytes)
        var uuidBytes = data.Slice(offset, 16);
        var id = new Guid(uuidBytes).ToString();
        offset += 16;

        // timestamp (i64 LE)
        var timestamp = BinaryPrimitives.ReadInt64LittleEndian(data.Slice(offset));
        offset += 8;

        // source (len-prefixed UTF-8)
        var sourceLen = (int)BinaryPrimitives.ReadUInt32LittleEndian(data.Slice(offset));
        offset += 4;
        var source = Encoding.UTF8.GetString(data.Slice(offset, sourceLen));
        offset += sourceLen;

        // partition (u16 LE)
        var partition = BinaryPrimitives.ReadUInt16LittleEndian(data.Slice(offset));
        offset += 2;

        // metadata (count + key/value pairs)
        var metaCount = (int)BinaryPrimitives.ReadUInt32LittleEndian(data.Slice(offset));
        offset += 4;
        var metadata = new List<string[]>(metaCount);
        for (int i = 0; i < metaCount; i++)
        {
            var keyLen = (int)BinaryPrimitives.ReadUInt32LittleEndian(data.Slice(offset));
            offset += 4;
            var key = Encoding.UTF8.GetString(data.Slice(offset, keyLen));
            offset += keyLen;
            var valLen = (int)BinaryPrimitives.ReadUInt32LittleEndian(data.Slice(offset));
            offset += 4;
            var val = Encoding.UTF8.GetString(data.Slice(offset, valLen));
            offset += valLen;
            metadata.Add(new[] { key, val });
        }

        // payload (len-prefixed)
        var payloadLen = (int)BinaryPrimitives.ReadUInt32LittleEndian(data.Slice(offset));
        offset += 4;
        var payload = data.Slice(offset, payloadLen).ToArray();

        return new Event
        {
            Id = id,
            Timestamp = timestamp,
            Source = source,
            Partition = partition,
            Metadata = metadata,
            Payload = payload,
        };
    }

    /// <summary>Serialize outputs to native wire format.</summary>
    public static byte[] SerializeOutputs(List<Output> outputs)
    {
        using var ms = new MemoryStream();
        Span<byte> buf = stackalloc byte[4];

        BinaryPrimitives.WriteUInt32LittleEndian(buf, (uint)outputs.Count);
        ms.Write(buf);

        foreach (var o in outputs)
        {
            // destination (len-prefixed)
            var destBytes = Encoding.UTF8.GetBytes(o.Destination);
            BinaryPrimitives.WriteUInt32LittleEndian(buf, (uint)destBytes.Length);
            ms.Write(buf);
            ms.Write(destBytes);

            // key (has_key flag + optional len-prefixed)
            if (o.Key != null)
            {
                ms.WriteByte(1);
                BinaryPrimitives.WriteUInt32LittleEndian(buf, (uint)o.Key.Length);
                ms.Write(buf);
                ms.Write(o.Key);
            }
            else
            {
                ms.WriteByte(0);
            }

            // payload (len-prefixed)
            BinaryPrimitives.WriteUInt32LittleEndian(buf, (uint)o.Payload.Length);
            ms.Write(buf);
            ms.Write(o.Payload);

            // headers (count + key/value pairs)
            BinaryPrimitives.WriteUInt32LittleEndian(buf, (uint)o.Headers.Count);
            ms.Write(buf);
            foreach (var h in o.Headers)
            {
                var keyBytes = Encoding.UTF8.GetBytes(h[0]);
                BinaryPrimitives.WriteUInt32LittleEndian(buf, (uint)keyBytes.Length);
                ms.Write(buf);
                ms.Write(keyBytes);
                var valBytes = Encoding.UTF8.GetBytes(h[1]);
                BinaryPrimitives.WriteUInt32LittleEndian(buf, (uint)valBytes.Length);
                ms.Write(buf);
                ms.Write(valBytes);
            }
        }

        return ms.ToArray();
    }
}
