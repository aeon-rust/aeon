using System.Buffers.Binary;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using System.Text;
using Aeon.ProcessorSdk;

namespace Aeon.PassthroughNative;

/// <summary>
/// Passthrough processor: each event produces one output with
/// destination="output" and the same payload.
/// Uses the NativeWire binary format expected by NativeProcessor.
/// </summary>
internal class PassthroughProcessor : INativeProcessor
{
    public byte[] Process(ReadOnlySpan<byte> eventData)
    {
        var evt = NativeWire.DeserializeEvent(eventData);
        var output = new Output
        {
            Destination = "output",
            Payload = evt.Payload,
            Headers = new List<string[]>(),
        };
        return NativeWire.SerializeOutputs(new List<Output> { output });
    }

    public byte[] ProcessBatch(ReadOnlySpan<byte> batchData)
    {
        int offset = 0;
        var count = (int)BitConverter.ToUInt32(batchData.Slice(offset, 4));
        offset += 4;

        var allOutputs = new List<Output>(count);
        for (int i = 0; i < count; i++)
        {
            var evLen = (int)BitConverter.ToUInt32(batchData.Slice(offset, 4));
            offset += 4;
            var evData = batchData.Slice(offset, evLen);
            offset += evLen;

            var evt = NativeWire.DeserializeEvent(evData);
            allOutputs.Add(new Output
            {
                Destination = "output",
                Payload = evt.Payload,
                Headers = new List<string[]>(),
            });
        }

        return NativeWire.SerializeOutputs(allOutputs);
    }
}

/// <summary>
/// Direct C-ABI exports for NativeAOT. These must be in the publishing
/// project to be exported from the native shared library.
/// </summary>
internal static class Exports
{
    private static PassthroughProcessor? _instance;
    private static GCHandle _instanceHandle;

    private static byte[]? _nameBytes;
    private static byte[]? _versionBytes;
    private static GCHandle _namePinned;
    private static GCHandle _versionPinned;

    [UnmanagedCallersOnly(EntryPoint = "aeon_processor_create")]
    public static unsafe nint Create(byte* configPtr, nuint configLen)
    {
        try
        {
            _instance = new PassthroughProcessor();
            _instanceHandle = GCHandle.Alloc(_instance);
            return (nint)_instanceHandle;
        }
        catch
        {
            return 0;
        }
    }

    [UnmanagedCallersOnly(EntryPoint = "aeon_processor_destroy")]
    public static void Destroy(nint ctx)
    {
        if (ctx != 0 && _instanceHandle.IsAllocated)
        {
            _instanceHandle.Free();
            _instance = null;
        }
    }

    [UnmanagedCallersOnly(EntryPoint = "aeon_process")]
    public static unsafe int Process(
        nint ctx,
        byte* eventPtr, nuint eventLen,
        byte* outBuf, nuint outCapacity,
        nuint* outLen)
    {
        if (eventPtr == null || outBuf == null || outLen == null || _instance == null)
            return -1;

        try
        {
            var eventData = new ReadOnlySpan<byte>(eventPtr, (int)eventLen);
            var result = _instance.Process(eventData);

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

    [UnmanagedCallersOnly(EntryPoint = "aeon_process_batch")]
    public static unsafe int ProcessBatch(
        nint ctx,
        byte* eventsPtr, nuint eventsLen,
        byte* outBuf, nuint outCapacity,
        nuint* outLen)
    {
        if (eventsPtr == null || outBuf == null || outLen == null || _instance == null)
            return -1;

        try
        {
            var batchData = new ReadOnlySpan<byte>(eventsPtr, (int)eventsLen);
            var result = _instance.ProcessBatch(batchData);

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

    [UnmanagedCallersOnly(EntryPoint = "aeon_processor_name")]
    public static unsafe byte* ProcessorName()
    {
        if (_nameBytes == null)
        {
            _nameBytes = Encoding.UTF8.GetBytes("dotnet-passthrough\0");
            _namePinned = GCHandle.Alloc(_nameBytes, GCHandleType.Pinned);
        }
        return (byte*)_namePinned.AddrOfPinnedObject();
    }

    [UnmanagedCallersOnly(EntryPoint = "aeon_processor_version")]
    public static unsafe byte* ProcessorVersion()
    {
        if (_versionBytes == null)
        {
            _versionBytes = Encoding.UTF8.GetBytes("1.0.0\0");
            _versionPinned = GCHandle.Alloc(_versionBytes, GCHandleType.Pinned);
        }
        return (byte*)_versionPinned.AddrOfPinnedObject();
    }
}
