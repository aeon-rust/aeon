// Aeon Processor SDK — Tests for C#/.NET implementation.
// Run: dotnet test

using System.Buffers.Binary;
using System.Text;
using Xunit;

namespace Aeon.ProcessorSdk.Tests;

// ── CRC32 Tests ──────────────────────────────────────────────────────────

public class Crc32Tests
{
    [Fact]
    public void EmptyBuffer_ReturnsZero()
    {
        Assert.Equal(0u, Crc32.Compute(ReadOnlySpan<byte>.Empty));
    }

    [Fact]
    public void KnownValue_123456789()
    {
        var data = Encoding.ASCII.GetBytes("123456789");
        Assert.Equal(0xCBF43926u, Crc32.Compute(data));
    }

    [Fact]
    public void BinaryData_ReturnsConsistentValue()
    {
        var data = new byte[] { 0x00, 0xFF, 0x55, 0xAA };
        var result = Crc32.Compute(data);
        Assert.True(result > 0);
        // Same input → same output
        Assert.Equal(result, Crc32.Compute(data));
    }
}

// ── Signer Tests ─────────────────────────────────────────────────────────

public class SignerTests
{
    [Fact]
    public void Generate_ProducesValidKeypair()
    {
        using var signer = Signer.Generate();
        Assert.Equal(64, signer.PublicKeyHex.Length);
        Assert.StartsWith("ed25519:", signer.PublicKeyFormatted);
        Assert.Equal(64, signer.Fingerprint.Length);
    }

    [Fact]
    public void FromSeed_DeterministicKeys()
    {
        var seed = new byte[32];
        Array.Fill(seed, (byte)0x42);
        using var s1 = Signer.FromSeed(seed);
        using var s2 = Signer.FromSeed(seed);
        Assert.Equal(s1.PublicKeyHex, s2.PublicKeyHex);
    }

    [Fact]
    public void DifferentSeeds_DifferentKeys()
    {
        var seed1 = new byte[32];
        var seed2 = new byte[32];
        Array.Fill(seed1, (byte)0x01);
        Array.Fill(seed2, (byte)0x02);
        using var s1 = Signer.FromSeed(seed1);
        using var s2 = Signer.FromSeed(seed2);
        Assert.NotEqual(s1.PublicKeyHex, s2.PublicKeyHex);
    }

    [Fact]
    public void SignChallenge_Returns128HexChars()
    {
        using var signer = Signer.Generate();
        var nonceHex = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        var sigHex = signer.SignChallenge(nonceHex);
        Assert.Equal(128, sigHex.Length); // 64 bytes = 128 hex chars
    }

    [Fact]
    public void SignAndVerifyBatch()
    {
        using var signer = Signer.Generate();
        var data = Encoding.UTF8.GetBytes("test batch data");
        var sig = signer.SignBatch(data);
        Assert.Equal(64, sig.Length);
        Assert.True(signer.Verify(data, sig));
    }

    [Fact]
    public void VerifyFailsWithWrongData()
    {
        using var signer = Signer.Generate();
        var data = Encoding.UTF8.GetBytes("original data");
        var sig = signer.SignBatch(data);
        var tampered = Encoding.UTF8.GetBytes("tampered data");
        Assert.False(signer.Verify(tampered, sig));
    }

    [Fact]
    public void InvalidSeedLength_Throws()
    {
        Assert.Throws<ArgumentException>(() => Signer.FromSeed(new byte[16]));
    }
}

// ── Codec Tests ──────────────────────────────────────────────────────────

public class CodecMsgPackTests : CodecTestBase
{
    public CodecMsgPackTests() : base(new Codec(CodecType.MsgPack)) { }
}

public class CodecJsonTests : CodecTestBase
{
    public CodecJsonTests() : base(new Codec(CodecType.Json)) { }
}

public abstract class CodecTestBase
{
    private readonly Codec _codec;

    protected CodecTestBase(Codec codec) => _codec = codec;

    [Fact]
    public void RoundtripEvent()
    {
        var evt = new Event
        {
            Id = "550e8400-e29b-41d4-a716-446655440000",
            Timestamp = 1712345678000000000,
            Source = "test-source",
            Partition = 3,
            Metadata = new() { new[] { "key1", "val1" } },
            Payload = Encoding.UTF8.GetBytes("hello world"),
            SourceOffset = 42,
        };

        var encoded = _codec.EncodeEvent(evt);
        var decoded = _codec.DecodeEvent(encoded);

        Assert.Equal(evt.Id, decoded.Id);
        Assert.Equal(evt.Timestamp, decoded.Timestamp);
        Assert.Equal(evt.Source, decoded.Source);
        Assert.Equal(evt.Partition, decoded.Partition);
        Assert.Single(decoded.Metadata);
        Assert.Equal("key1", decoded.Metadata[0][0]);
        Assert.Equal("val1", decoded.Metadata[0][1]);
        Assert.Equal(evt.Payload, decoded.Payload);
        Assert.Equal(42L, decoded.SourceOffset);
    }

    [Fact]
    public void RoundtripEvent_WithoutOptionalFields()
    {
        var evt = new Event
        {
            Id = "550e8400-e29b-41d4-a716-446655440000",
            Timestamp = 0,
            Source = "src",
            Partition = 0,
            Metadata = new(),
            Payload = Array.Empty<byte>(),
            SourceOffset = null,
        };

        var encoded = _codec.EncodeEvent(evt);
        var decoded = _codec.DecodeEvent(encoded);

        Assert.Equal(evt.Id, decoded.Id);
        Assert.Null(decoded.SourceOffset);
    }

    [Fact]
    public void RoundtripOutput()
    {
        var output = new Output
        {
            Destination = "sink-topic",
            Payload = Encoding.UTF8.GetBytes("output data"),
            Key = Encoding.UTF8.GetBytes("my-key"),
            Headers = new() { new[] { "h1", "v1" }, new[] { "h2", "v2" } },
            SourceEventId = "550e8400-e29b-41d4-a716-446655440000",
            SourcePartition = 5,
            SourceOffset = 99,
        };

        var encoded = _codec.EncodeOutput(output);
        var decoded = _codec.DecodeOutput(encoded);

        Assert.Equal(output.Destination, decoded.Destination);
        Assert.Equal(output.Payload, decoded.Payload);
        Assert.Equal(output.Key, decoded.Key);
        Assert.Equal(2, decoded.Headers.Count);
        Assert.Equal(output.SourceEventId, decoded.SourceEventId);
        Assert.Equal(5, decoded.SourcePartition);
        Assert.Equal(99L, decoded.SourceOffset);
    }

    [Fact]
    public void RoundtripOutput_WithoutKey()
    {
        var output = new Output
        {
            Destination = "out",
            Payload = Encoding.UTF8.GetBytes("data"),
            Key = null,
            Headers = new(),
            SourceEventId = null,
            SourcePartition = null,
            SourceOffset = null,
        };

        var encoded = _codec.EncodeOutput(output);
        var decoded = _codec.DecodeOutput(encoded);

        Assert.Equal("out", decoded.Destination);
        Assert.Null(decoded.Key);
        Assert.Null(decoded.SourceEventId);
    }

    [Fact]
    public void RoundtripEvent_BinaryPayload()
    {
        var evt = new Event
        {
            Id = "test-binary",
            Timestamp = 1000,
            Source = "bin",
            Partition = 0,
            Payload = new byte[] { 0x00, 0xFF, 0x55, 0xAA, 0x01, 0x02, 0x03 },
        };

        var encoded = _codec.EncodeEvent(evt);
        var decoded = _codec.DecodeEvent(encoded);
        Assert.Equal(evt.Payload, decoded.Payload);
    }
}

// ── Data Frame Tests ─────────────────────────────────────────────────────

public class DataFrameTests
{
    [Fact]
    public void RoundtripDataFrame()
    {
        var name = "my-pipeline";
        var partition = 7;
        var batchData = Encoding.UTF8.GetBytes("batch-content");

        var frame = DataFrameWire.Build(name, partition, batchData);
        var parsed = DataFrameWire.Parse(frame);

        Assert.Equal(name, parsed.PipelineName);
        Assert.Equal(partition, parsed.Partition);
        Assert.Equal(batchData, parsed.BatchData);
    }

    [Fact]
    public void EmptyPipelineName()
    {
        var frame = DataFrameWire.Build("", 0, Encoding.UTF8.GetBytes("data"));
        var parsed = DataFrameWire.Parse(frame);
        Assert.Equal("", parsed.PipelineName);
        Assert.Equal(0, parsed.Partition);
    }

    [Fact]
    public void UnicodePipelineName()
    {
        var name = "pipeline-\u65E5\u672C\u8A9E";
        var frame = DataFrameWire.Build(name, 100, new byte[] { 0x78 });
        var parsed = DataFrameWire.Parse(frame);
        Assert.Equal(name, parsed.PipelineName);
    }

    [Fact]
    public void TooShortFrame_Throws()
    {
        Assert.Throws<InvalidOperationException>(() => DataFrameWire.Parse(new byte[3]));
    }
}

// ── Batch Wire Format Tests ──────────────────────────────────────────────

public class BatchWireTests
{
    [Fact]
    public void EncodeAndVerifySignedBatchResponse()
    {
        var codec = new Codec(CodecType.MsgPack);
        using var signer = Signer.Generate();

        var outputsPerEvent = new List<List<Output>>
        {
            new() { new Output { Destination = "out", Payload = Encoding.UTF8.GetBytes("hello") } },
            new(), // event with no outputs
        };

        var response = BatchWire.EncodeBatchResponse(42, outputsPerEvent, codec, signer, true);

        // Verify structure: header(12) + outputs + CRC(4) + sig(64)
        Assert.True(response.Length > 12 + 4 + 64);

        // Verify batch_id
        Assert.Equal(42UL, BinaryPrimitives.ReadUInt64LittleEndian(response));
        // Verify event_count
        Assert.Equal(2u, BinaryPrimitives.ReadUInt32LittleEndian(response.AsSpan(8)));

        // Verify CRC32
        var payloadEnd = response.Length - 64 - 4;
        var crcExpected = BinaryPrimitives.ReadUInt32LittleEndian(response.AsSpan(payloadEnd));
        var crcActual = Crc32.Compute(response.AsSpan(0, payloadEnd));
        Assert.Equal(crcExpected, crcActual);

        // Verify signature
        var sigStart = response.Length - 64;
        var signedData = response.AsSpan(0, sigStart);
        var sig = response.AsSpan(sigStart);
        Assert.True(signer.Verify(signedData, sig));
    }

    [Fact]
    public void EncodeUnsignedBatchResponse()
    {
        var codec = new Codec(CodecType.MsgPack);

        var response = BatchWire.EncodeBatchResponse(
            1,
            new() { new() { new Output { Destination = "out", Payload = new byte[] { 0x78 } } } },
            codec, null, false);

        // Last 64 bytes should be zeros
        var sig = response.AsSpan(response.Length - 64);
        Assert.True(sig.SequenceEqual(new byte[64]));
    }

    [Fact]
    public void RoundtripBatchRequest()
    {
        var codec = new Codec(CodecType.MsgPack);

        var batchId = 99UL;
        var events = new List<Event>
        {
            new()
            {
                Id = "abc-123",
                Timestamp = 1000,
                Source = "src",
                Partition = 0,
                Metadata = new(),
                Payload = Encoding.UTF8.GetBytes("event-data"),
                SourceOffset = null,
            },
        };

        // Build request manually
        var header = new byte[12];
        BinaryPrimitives.WriteUInt64LittleEndian(header, batchId);
        BinaryPrimitives.WriteUInt32LittleEndian(header.AsSpan(8), (uint)events.Count);

        using var ms = new MemoryStream();
        ms.Write(header);
        foreach (var evt in events)
        {
            var encoded = codec.EncodeEvent(evt);
            var lenBuf = new byte[4];
            BinaryPrimitives.WriteUInt32LittleEndian(lenBuf, (uint)encoded.Length);
            ms.Write(lenBuf);
            ms.Write(encoded);
        }

        var payload = ms.ToArray();
        var crcVal = Crc32.Compute(payload);
        var crcBuf = new byte[4];
        BinaryPrimitives.WriteUInt32LittleEndian(crcBuf, crcVal);

        var request = new byte[payload.Length + 4];
        Buffer.BlockCopy(payload, 0, request, 0, payload.Length);
        Buffer.BlockCopy(crcBuf, 0, request, payload.Length, 4);

        // Decode
        var decoded = BatchWire.DecodeBatchRequest(request, codec);
        Assert.Equal(batchId, decoded.BatchId);
        Assert.Single(decoded.Events);
        Assert.Equal("abc-123", decoded.Events[0].Id);
        Assert.Equal(Encoding.UTF8.GetBytes("event-data"), decoded.Events[0].Payload);
    }

    [Fact]
    public void CrcMismatch_Throws()
    {
        var codec = new Codec(CodecType.MsgPack);

        var header = new byte[12];
        BinaryPrimitives.WriteUInt64LittleEndian(header, 1);
        BinaryPrimitives.WriteUInt32LittleEndian(header.AsSpan(8), 0);
        var badCrc = new byte[4];
        BinaryPrimitives.WriteUInt32LittleEndian(badCrc, 0xDEADBEEF);
        var request = new byte[16];
        Buffer.BlockCopy(header, 0, request, 0, 12);
        Buffer.BlockCopy(badCrc, 0, request, 12, 4);

        var ex = Assert.Throws<InvalidOperationException>(() => BatchWire.DecodeBatchRequest(request, codec));
        Assert.Contains("CRC32 mismatch", ex.Message);
    }

    [Fact]
    public void MultipleEventsInBatch()
    {
        var codec = new Codec(CodecType.Json);

        var batchId = 7UL;
        var events = Enumerable.Range(0, 5).Select(i => new Event
        {
            Id = $"event-{i}",
            Timestamp = i * 1000,
            Source = "src",
            Partition = i % 4,
            Metadata = new() { new[] { "idx", $"{i}" } },
            Payload = Encoding.UTF8.GetBytes($"payload-{i}"),
            SourceOffset = i,
        }).ToList();

        // Build request
        var header = new byte[12];
        BinaryPrimitives.WriteUInt64LittleEndian(header, batchId);
        BinaryPrimitives.WriteUInt32LittleEndian(header.AsSpan(8), (uint)events.Count);

        using var ms = new MemoryStream();
        ms.Write(header);
        foreach (var evt in events)
        {
            var encoded = codec.EncodeEvent(evt);
            var lenBuf = new byte[4];
            BinaryPrimitives.WriteUInt32LittleEndian(lenBuf, (uint)encoded.Length);
            ms.Write(lenBuf);
            ms.Write(encoded);
        }

        var payload = ms.ToArray();
        var crcBuf = new byte[4];
        BinaryPrimitives.WriteUInt32LittleEndian(crcBuf, Crc32.Compute(payload));
        var request = new byte[payload.Length + 4];
        Buffer.BlockCopy(payload, 0, request, 0, payload.Length);
        Buffer.BlockCopy(crcBuf, 0, request, payload.Length, 4);

        var decoded = BatchWire.DecodeBatchRequest(request, codec);
        Assert.Equal(batchId, decoded.BatchId);
        Assert.Equal(5, decoded.Events.Count);
        for (int i = 0; i < 5; i++)
        {
            Assert.Equal($"event-{i}", decoded.Events[i].Id);
            Assert.Equal(i % 4, decoded.Events[i].Partition);
            Assert.Equal((long)i, decoded.Events[i].SourceOffset);
        }
    }
}

// ── Processor Registration Tests ─────────────────────────────────────────

public class ProcessorRegistrationTests
{
    [Fact]
    public void PerEventProcessor()
    {
        var p = ProcessorRegistration.PerEvent(e =>
            new() { new Output { Destination = "out", Payload = e.Payload } });
        Assert.NotNull(p.ProcessFn);
        Assert.Null(p.BatchProcessFn);
    }

    [Fact]
    public void BatchProcessor()
    {
        var p = ProcessorRegistration.Batch(events =>
            events.Select(e => new List<Output> { new() { Destination = "out", Payload = e.Payload } }).ToList());
        Assert.Null(p.ProcessFn);
        Assert.NotNull(p.BatchProcessFn);
    }

    [Fact]
    public void OutputWithEventIdentity()
    {
        var evt = new Event { Id = "evt-1", Partition = 3, SourceOffset = 42 };
        var output = new Output { Destination = "out", Payload = new byte[] { 0x78 } };
        output.WithEventIdentity(evt);
        Assert.Equal("evt-1", output.SourceEventId);
        Assert.Equal(3, output.SourcePartition);
        Assert.Equal(42L, output.SourceOffset);
    }

    [Fact]
    public void PerEventProcessor_ReturnsCorrectOutputs()
    {
        var p = ProcessorRegistration.PerEvent(e =>
            new() { new Output { Destination = "out", Payload = e.Payload } });
        var evt = new Event
        {
            Id = "test",
            Payload = Encoding.UTF8.GetBytes("hello"),
        };
        var outputs = p.ProcessFn!(evt);
        Assert.Single(outputs);
        Assert.Equal("out", outputs[0].Destination);
        Assert.Equal(Encoding.UTF8.GetBytes("hello"), outputs[0].Payload);
    }
}

// ── Cross-Codec Compatibility Tests ──────────────────────────────────────

public class CrossCodecTests
{
    [Theory]
    [InlineData(CodecType.MsgPack)]
    [InlineData(CodecType.Json)]
    public void BatchWireFormat_ValidAcrossCodecs(CodecType codecType)
    {
        var codec = new Codec(codecType);
        using var signer = Signer.Generate();

        var outputs = new List<List<Output>>
        {
            new()
            {
                new Output
                {
                    Destination = "topic-out",
                    Payload = Encoding.UTF8.GetBytes("binary-data"),
                    Key = Encoding.UTF8.GetBytes("key-1"),
                    Headers = new() { new[] { "content-type", "application/octet-stream" } },
                },
            },
        };

        var response = BatchWire.EncodeBatchResponse(1, outputs, codec, signer, true);

        // Verify batch_id
        Assert.Equal(1UL, BinaryPrimitives.ReadUInt64LittleEndian(response));
        // Verify event_count
        Assert.Equal(1u, BinaryPrimitives.ReadUInt32LittleEndian(response.AsSpan(8)));

        // Verify CRC
        var crcOffset = response.Length - 64 - 4;
        var expectedCrc = BinaryPrimitives.ReadUInt32LittleEndian(response.AsSpan(crcOffset));
        var actualCrc = Crc32.Compute(response.AsSpan(0, crcOffset));
        Assert.Equal(expectedCrc, actualCrc);

        // Verify signature
        var signedData = response.AsSpan(0, response.Length - 64);
        var sig = response.AsSpan(response.Length - 64);
        Assert.True(signer.Verify(signedData, sig));
    }
}

// ── Native Wire Format Tests ─────────────────────────────────────────────

public class NativeWireTests
{
    [Fact]
    public void SerializeAndDeserializeEvent()
    {
        var original = new Event
        {
            Id = "550e8400-e29b-41d4-a716-446655440000",
            Timestamp = 1712345678000000000,
            Source = "test-source",
            Partition = 3,
            Metadata = new() { new[] { "key1", "val1" } },
            Payload = Encoding.UTF8.GetBytes("hello world"),
        };

        // Build native wire format manually
        using var ms = new MemoryStream();
        var buf = new byte[8];

        // UUID (16 bytes)
        ms.Write(Guid.Parse(original.Id).ToByteArray());

        // timestamp
        BinaryPrimitives.WriteInt64LittleEndian(buf, original.Timestamp);
        ms.Write(buf, 0, 8);

        // source
        var srcBytes = Encoding.UTF8.GetBytes(original.Source);
        BinaryPrimitives.WriteUInt32LittleEndian(buf, (uint)srcBytes.Length);
        ms.Write(buf, 0, 4);
        ms.Write(srcBytes);

        // partition
        BinaryPrimitives.WriteUInt16LittleEndian(buf, (ushort)original.Partition);
        ms.Write(buf, 0, 2);

        // metadata
        BinaryPrimitives.WriteUInt32LittleEndian(buf, (uint)original.Metadata.Count);
        ms.Write(buf, 0, 4);
        foreach (var pair in original.Metadata)
        {
            var kb = Encoding.UTF8.GetBytes(pair[0]);
            BinaryPrimitives.WriteUInt32LittleEndian(buf, (uint)kb.Length);
            ms.Write(buf, 0, 4);
            ms.Write(kb);
            var vb = Encoding.UTF8.GetBytes(pair[1]);
            BinaryPrimitives.WriteUInt32LittleEndian(buf, (uint)vb.Length);
            ms.Write(buf, 0, 4);
            ms.Write(vb);
        }

        // payload
        BinaryPrimitives.WriteUInt32LittleEndian(buf, (uint)original.Payload.Length);
        ms.Write(buf, 0, 4);
        ms.Write(original.Payload);

        var wireData = ms.ToArray();
        var decoded = NativeWire.DeserializeEvent(wireData);

        Assert.Equal(original.Id, decoded.Id);
        Assert.Equal(original.Timestamp, decoded.Timestamp);
        Assert.Equal(original.Source, decoded.Source);
        Assert.Equal(original.Partition, decoded.Partition);
        Assert.Single(decoded.Metadata);
        Assert.Equal("key1", decoded.Metadata[0][0]);
        Assert.Equal("val1", decoded.Metadata[0][1]);
        Assert.Equal(original.Payload, decoded.Payload);
    }

    [Fact]
    public void SerializeOutputs_Roundtrip()
    {
        var outputs = new List<Output>
        {
            new()
            {
                Destination = "sink-topic",
                Payload = Encoding.UTF8.GetBytes("output data"),
                Key = Encoding.UTF8.GetBytes("key-1"),
                Headers = new() { new[] { "h1", "v1" } },
            },
            new()
            {
                Destination = "out-2",
                Payload = new byte[] { 0x00, 0xFF },
                Key = null,
                Headers = new(),
            },
        };

        var wire = NativeWire.SerializeOutputs(outputs);

        // Verify manually: read output_count
        int offset = 0;
        var count = BinaryPrimitives.ReadUInt32LittleEndian(wire.AsSpan(offset));
        Assert.Equal(2u, count);
        offset += 4;

        // First output: destination
        var destLen = (int)BinaryPrimitives.ReadUInt32LittleEndian(wire.AsSpan(offset));
        offset += 4;
        Assert.Equal("sink-topic", Encoding.UTF8.GetString(wire, offset, destLen));
        offset += destLen;

        // has_key = 1
        Assert.Equal(1, wire[offset]);
        offset += 1;
        var keyLen = (int)BinaryPrimitives.ReadUInt32LittleEndian(wire.AsSpan(offset));
        offset += 4;
        Assert.Equal("key-1", Encoding.UTF8.GetString(wire, offset, keyLen));
        offset += keyLen;

        // payload
        var payloadLen = (int)BinaryPrimitives.ReadUInt32LittleEndian(wire.AsSpan(offset));
        offset += 4;
        Assert.Equal(Encoding.UTF8.GetBytes("output data"), wire.AsSpan(offset, payloadLen).ToArray());
        offset += payloadLen;

        // headers: 1
        var headerCount = BinaryPrimitives.ReadUInt32LittleEndian(wire.AsSpan(offset));
        Assert.Equal(1u, headerCount);
    }

    [Fact]
    public void SerializeOutputs_EmptyList()
    {
        var wire = NativeWire.SerializeOutputs(new List<Output>());
        Assert.Equal(4, wire.Length); // just the count (0)
        Assert.Equal(0u, BinaryPrimitives.ReadUInt32LittleEndian(wire));
    }
}

// ── Runner Config Tests ──────────────────────────────────────────────────

public class RunnerConfigTests
{
    [Fact]
    public void DefaultConfig()
    {
        var config = new RunnerConfig();
        Assert.Equal("dotnet-processor", config.Name);
        Assert.Equal("1.0.0", config.Version);
        Assert.Equal("msgpack", config.CodecName);
        Assert.Equal("dedicated", config.Binding);
        Assert.Equal(1000, config.MaxBatchSize);
    }

    [Fact]
    public void CodecNameMapping()
    {
        Assert.Equal("msgpack", new Codec("msgpack").Name);
        Assert.Equal("json", new Codec("json").Name);
        Assert.Equal("json", new Codec("JSON").Name);
        Assert.Equal("msgpack", new Codec("MsgPack").Name);
    }
}
