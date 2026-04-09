// Aeon Processor SDK — Wire format: CRC32, batch encode/decode, data frame.

using System.Buffers.Binary;
using System.Text;

namespace Aeon.ProcessorSdk;

/// <summary>
/// CRC32 (IEEE) implementation matching the AWPP batch wire protocol.
/// Polynomial: 0xEDB88320 (reflected).
/// </summary>
public static class Crc32
{
    private static readonly uint[] Table = BuildTable();

    private static uint[] BuildTable()
    {
        var table = new uint[256];
        for (uint i = 0; i < 256; i++)
        {
            var c = i;
            for (int j = 0; j < 8; j++)
                c = (c & 1) != 0 ? (0xEDB88320u ^ (c >> 1)) : (c >> 1);
            table[i] = c;
        }
        return table;
    }

    /// <summary>Compute CRC32 (IEEE) of a byte span.</summary>
    public static uint Compute(ReadOnlySpan<byte> data)
    {
        uint crc = 0xFFFFFFFF;
        foreach (var b in data)
            crc = Table[(crc ^ b) & 0xFF] ^ (crc >> 8);
        return crc ^ 0xFFFFFFFF;
    }
}

/// <summary>Decoded batch request.</summary>
public sealed class BatchRequest
{
    public ulong BatchId { get; set; }
    public List<Event> Events { get; set; } = new();
}

/// <summary>
/// Batch wire format encoder/decoder for the AWPP protocol.
///
/// Request:  [8B batch_id LE][4B count LE][per event: 4B len LE + codec bytes][4B CRC32 LE]
/// Response: [8B batch_id LE][4B count LE][per event: 4B output_count LE [per output: 4B len LE + codec bytes]][4B CRC32 LE][64B ED25519 sig]
/// </summary>
public static class BatchWire
{
    /// <summary>Decode a batch request from binary wire format.</summary>
    public static BatchRequest DecodeBatchRequest(ReadOnlySpan<byte> data, Codec codec)
    {
        if (data.Length < 16)
            throw new InvalidOperationException($"Batch request too short: {data.Length} bytes");

        var batchId = BinaryPrimitives.ReadUInt64LittleEndian(data);
        var eventCount = BinaryPrimitives.ReadUInt32LittleEndian(data.Slice(8));

        // Verify CRC32: covers everything except the last 4 bytes
        int payloadEnd = data.Length - 4;
        var expectedCrc = BinaryPrimitives.ReadUInt32LittleEndian(data.Slice(payloadEnd));
        var actualCrc = Crc32.Compute(data.Slice(0, payloadEnd));
        if (expectedCrc != actualCrc)
            throw new InvalidOperationException($"CRC32 mismatch: expected {expectedCrc}, got {actualCrc}");

        var events = new List<Event>((int)eventCount);
        int offset = 12;
        for (int i = 0; i < eventCount; i++)
        {
            if (offset + 4 > payloadEnd)
                throw new InvalidOperationException($"Unexpected end of batch at event {i}");
            var eventLen = (int)BinaryPrimitives.ReadUInt32LittleEndian(data.Slice(offset));
            offset += 4;
            if (offset + eventLen > payloadEnd)
                throw new InvalidOperationException($"Event {i} extends beyond batch");
            events.Add(codec.DecodeEvent(data.Slice(offset, eventLen)));
            offset += eventLen;
        }

        return new BatchRequest { BatchId = batchId, Events = events };
    }

    /// <summary>Encode a batch response to binary wire format.</summary>
    public static byte[] EncodeBatchResponse(
        ulong batchId,
        List<List<Output>> outputsPerEvent,
        Codec codec,
        Signer? signer,
        bool batchSigning)
    {
        using var ms = new MemoryStream();

        // Header: batch_id + event_count
        Span<byte> header = stackalloc byte[12];
        BinaryPrimitives.WriteUInt64LittleEndian(header, batchId);
        BinaryPrimitives.WriteUInt32LittleEndian(header.Slice(8), (uint)outputsPerEvent.Count);
        ms.Write(header);

        // Per-event outputs
        Span<byte> u32Buf = stackalloc byte[4];
        foreach (var outputs in outputsPerEvent)
        {
            BinaryPrimitives.WriteUInt32LittleEndian(u32Buf, (uint)outputs.Count);
            ms.Write(u32Buf);
            foreach (var output in outputs)
            {
                var encoded = codec.EncodeOutput(output);
                BinaryPrimitives.WriteUInt32LittleEndian(u32Buf, (uint)encoded.Length);
                ms.Write(u32Buf);
                ms.Write(encoded);
            }
        }

        var payload = ms.ToArray();

        // CRC32
        var crc = Crc32.Compute(payload);
        var crcBuf = new byte[4];
        BinaryPrimitives.WriteUInt32LittleEndian(crcBuf, crc);

        // Signature
        byte[] sig;
        if (batchSigning && signer != null)
        {
            // Sign payload + CRC
            var toSign = new byte[payload.Length + 4];
            Buffer.BlockCopy(payload, 0, toSign, 0, payload.Length);
            Buffer.BlockCopy(crcBuf, 0, toSign, payload.Length, 4);
            sig = signer.SignBatch(toSign);
        }
        else
        {
            sig = new byte[64]; // 64 zero bytes
        }

        // Concatenate: payload + CRC + sig
        var result = new byte[payload.Length + 4 + 64];
        Buffer.BlockCopy(payload, 0, result, 0, payload.Length);
        Buffer.BlockCopy(crcBuf, 0, result, payload.Length, 4);
        Buffer.BlockCopy(sig, 0, result, payload.Length + 4, 64);
        return result;
    }
}

/// <summary>Parsed data frame.</summary>
public sealed class DataFrame
{
    public string PipelineName { get; set; } = "";
    public int Partition { get; set; }
    public byte[] BatchData { get; set; } = Array.Empty<byte>();
}

/// <summary>
/// T4 WebSocket data frame with routing header.
/// Format: [4B name_len LE][pipeline_name UTF-8][2B partition LE][batch_data]
/// </summary>
public static class DataFrameWire
{
    /// <summary>Build a data frame with routing header.</summary>
    public static byte[] Build(string pipelineName, int partition, ReadOnlySpan<byte> batchData)
    {
        var nameBytes = Encoding.UTF8.GetBytes(pipelineName);
        var frame = new byte[4 + nameBytes.Length + 2 + batchData.Length];
        BinaryPrimitives.WriteUInt32LittleEndian(frame, (uint)nameBytes.Length);
        Buffer.BlockCopy(nameBytes, 0, frame, 4, nameBytes.Length);
        BinaryPrimitives.WriteUInt16LittleEndian(frame.AsSpan(4 + nameBytes.Length), (ushort)partition);
        batchData.CopyTo(frame.AsSpan(4 + nameBytes.Length + 2));
        return frame;
    }

    /// <summary>Parse a data frame routing header.</summary>
    public static DataFrame Parse(ReadOnlySpan<byte> data)
    {
        if (data.Length < 6)
            throw new InvalidOperationException($"Data frame too short: {data.Length}");

        var nameLen = (int)BinaryPrimitives.ReadUInt32LittleEndian(data);
        if (4 + nameLen + 2 > data.Length)
            throw new InvalidOperationException("Data frame name extends beyond buffer");

        var pipelineName = Encoding.UTF8.GetString(data.Slice(4, nameLen));
        var partition = BinaryPrimitives.ReadUInt16LittleEndian(data.Slice(4 + nameLen));
        var batchData = data.Slice(4 + nameLen + 2).ToArray();

        return new DataFrame
        {
            PipelineName = pipelineName,
            Partition = partition,
            BatchData = batchData,
        };
    }
}
