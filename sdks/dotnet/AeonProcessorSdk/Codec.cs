// Aeon Processor SDK — MsgPack + JSON codec for Event/Output serialization.

using System.Text.Json;
using MessagePack;

namespace Aeon.ProcessorSdk;

public enum CodecType { MsgPack, Json }

/// <summary>
/// Encodes/decodes Event and Output using MsgPack or JSON wire format.
/// Field names match the AWPP protocol: id, timestamp, source, partition,
/// metadata, payload, source_offset, destination, key, headers,
/// source_event_id, source_partition.
/// </summary>
public sealed class Codec
{
    public CodecType Type { get; }

    public Codec(CodecType type = CodecType.MsgPack) => Type = type;
    public Codec(string name)
    {
        Type = name.Equals("json", StringComparison.OrdinalIgnoreCase)
            ? CodecType.Json : CodecType.MsgPack;
    }

    public string Name => Type == CodecType.Json ? "json" : "msgpack";

    // ── Event ────────────────────────────────────────────────────────

    public byte[] EncodeEvent(Event e)
    {
        if (Type == CodecType.MsgPack)
        {
            var dict = new Dictionary<string, object?>
            {
                ["id"] = e.Id,
                ["timestamp"] = e.Timestamp,
                ["source"] = e.Source,
                ["partition"] = e.Partition,
                ["metadata"] = e.Metadata,
                ["payload"] = e.Payload,
            };
            if (e.SourceOffset.HasValue)
                dict["source_offset"] = e.SourceOffset.Value;
            return MessagePackSerializer.Serialize(dict, MessagePack.Resolvers.ContractlessStandardResolver.Options);
        }
        else
        {
            var dict = new Dictionary<string, object?>
            {
                ["id"] = e.Id,
                ["timestamp"] = e.Timestamp,
                ["source"] = e.Source,
                ["partition"] = e.Partition,
                ["metadata"] = e.Metadata,
                ["payload"] = Convert.ToBase64String(e.Payload),
            };
            if (e.SourceOffset.HasValue)
                dict["source_offset"] = e.SourceOffset.Value;
            return System.Text.Encoding.UTF8.GetBytes(JsonSerializer.Serialize(dict));
        }
    }

    public Event DecodeEvent(ReadOnlySpan<byte> data)
    {
        if (Type == CodecType.MsgPack)
        {
            var dict = MessagePackSerializer.Deserialize<Dictionary<string, object>>(
                data.ToArray(), MessagePack.Resolvers.ContractlessStandardResolver.Options);
            return MapToEvent(dict);
        }
        else
        {
            using var doc = JsonDocument.Parse(data.ToArray());
            return JsonToEvent(doc.RootElement);
        }
    }

    // ── Output ───────────────────────────────────────────────────────

    public byte[] EncodeOutput(Output o)
    {
        if (Type == CodecType.MsgPack)
        {
            var dict = new Dictionary<string, object?>
            {
                ["destination"] = o.Destination,
                ["payload"] = o.Payload,
                ["headers"] = o.Headers,
            };
            if (o.Key != null) dict["key"] = o.Key;
            if (o.SourceEventId != null) dict["source_event_id"] = o.SourceEventId;
            if (o.SourcePartition.HasValue) dict["source_partition"] = o.SourcePartition.Value;
            if (o.SourceOffset.HasValue) dict["source_offset"] = o.SourceOffset.Value;
            return MessagePackSerializer.Serialize(dict, MessagePack.Resolvers.ContractlessStandardResolver.Options);
        }
        else
        {
            var dict = new Dictionary<string, object?>
            {
                ["destination"] = o.Destination,
                ["payload"] = PayloadToJsonArray(o.Payload),
                ["headers"] = o.Headers,
            };
            if (o.Key != null) dict["key"] = PayloadToJsonArray(o.Key);
            if (o.SourceEventId != null) dict["source_event_id"] = o.SourceEventId;
            if (o.SourcePartition.HasValue) dict["source_partition"] = o.SourcePartition.Value;
            if (o.SourceOffset.HasValue) dict["source_offset"] = o.SourceOffset.Value;
            return System.Text.Encoding.UTF8.GetBytes(JsonSerializer.Serialize(dict));
        }
    }

    public Output DecodeOutput(ReadOnlySpan<byte> data)
    {
        if (Type == CodecType.MsgPack)
        {
            var dict = MessagePackSerializer.Deserialize<Dictionary<string, object>>(
                data.ToArray(), MessagePack.Resolvers.ContractlessStandardResolver.Options);
            return MapToOutput(dict);
        }
        else
        {
            using var doc = JsonDocument.Parse(data.ToArray());
            return JsonToOutput(doc.RootElement);
        }
    }

    // ── MsgPack helpers ──────────────────────────────────────────────

    private static Event MapToEvent(Dictionary<string, object> d)
    {
        var e = new Event
        {
            Id = Convert.ToString(d["id"]) ?? "",
            Timestamp = Convert.ToInt64(d["timestamp"]),
            Source = Convert.ToString(d["source"]) ?? "",
            Partition = Convert.ToInt32(d["partition"]),
            Payload = ToBytes(d["payload"]),
        };
        if (d.TryGetValue("metadata", out var meta) && meta != null)
            e.Metadata = ToMetadata(meta);
        if (d.TryGetValue("source_offset", out var so) && so != null)
            e.SourceOffset = Convert.ToInt64(so);
        return e;
    }

    private static Output MapToOutput(Dictionary<string, object> d)
    {
        var o = new Output
        {
            Destination = Convert.ToString(d["destination"]) ?? "",
            Payload = ToBytes(d["payload"]),
        };
        if (d.TryGetValue("headers", out var h) && h != null)
            o.Headers = ToMetadata(h);
        if (d.TryGetValue("key", out var k) && k != null)
            o.Key = ToBytes(k);
        if (d.TryGetValue("source_event_id", out var sei) && sei != null)
            o.SourceEventId = Convert.ToString(sei);
        if (d.TryGetValue("source_partition", out var sp) && sp != null)
            o.SourcePartition = Convert.ToInt32(sp);
        if (d.TryGetValue("source_offset", out var soff) && soff != null)
            o.SourceOffset = Convert.ToInt64(soff);
        return o;
    }

    private static byte[] ToBytes(object val) => val switch
    {
        byte[] b => b,
        string s => Convert.FromBase64String(s),
        object?[] arr => ArrayToBytes(arr),
        _ => Array.Empty<byte>(),
    };

    private static byte[] ArrayToBytes(object?[] arr)
    {
        var result = new byte[arr.Length];
        for (int i = 0; i < arr.Length; i++)
            result[i] = (byte)Convert.ToInt32(arr[i]);
        return result;
    }

    private static List<string[]> ToMetadata(object val)
    {
        var result = new List<string[]>();
        if (val is object?[] arr)
        {
            foreach (var item in arr)
            {
                if (item is object?[] pair && pair.Length >= 2)
                    result.Add(new[] { Convert.ToString(pair[0]) ?? "", Convert.ToString(pair[1]) ?? "" });
            }
        }
        return result;
    }

    // ── JSON helpers ─────────────────────────────────────────────────

    private static Event JsonToEvent(JsonElement root)
    {
        var e = new Event
        {
            Id = root.GetProperty("id").GetString() ?? "",
            Timestamp = root.GetProperty("timestamp").GetInt64(),
            Source = root.GetProperty("source").GetString() ?? "",
            Partition = root.GetProperty("partition").GetInt32(),
            Payload = DecodeJsonPayload(root.GetProperty("payload")),
        };
        if (root.TryGetProperty("metadata", out var meta))
        {
            foreach (var pair in meta.EnumerateArray())
            {
                var items = pair.EnumerateArray().ToArray();
                if (items.Length >= 2)
                    e.Metadata.Add(new[] { items[0].GetString() ?? "", items[1].GetString() ?? "" });
            }
        }
        if (root.TryGetProperty("source_offset", out var so) && so.ValueKind != JsonValueKind.Null)
            e.SourceOffset = so.GetInt64();
        return e;
    }

    private static Output JsonToOutput(JsonElement root)
    {
        var o = new Output
        {
            Destination = root.GetProperty("destination").GetString() ?? "",
            Payload = DecodeJsonPayload(root.GetProperty("payload")),
        };
        if (root.TryGetProperty("headers", out var h))
        {
            foreach (var pair in h.EnumerateArray())
            {
                var items = pair.EnumerateArray().ToArray();
                if (items.Length >= 2)
                    o.Headers.Add(new[] { items[0].GetString() ?? "", items[1].GetString() ?? "" });
            }
        }
        if (root.TryGetProperty("key", out var k) && k.ValueKind != JsonValueKind.Null)
            o.Key = DecodeJsonPayload(k);
        if (root.TryGetProperty("source_event_id", out var sei) && sei.ValueKind != JsonValueKind.Null)
            o.SourceEventId = sei.GetString();
        if (root.TryGetProperty("source_partition", out var sp) && sp.ValueKind != JsonValueKind.Null)
            o.SourcePartition = sp.GetInt32();
        if (root.TryGetProperty("source_offset", out var soff) && soff.ValueKind != JsonValueKind.Null)
            o.SourceOffset = soff.GetInt64();
        return o;
    }

    /// <summary>
    /// Decode a JSON payload that may be either a base64 string (SDK encoding)
    /// or a JSON array of byte values (engine's serde encoding for Bytes/Vec&lt;u8&gt;).
    /// </summary>
    private static byte[] DecodeJsonPayload(JsonElement el)
    {
        if (el.ValueKind == JsonValueKind.String)
            return Convert.FromBase64String(el.GetString() ?? "");
        if (el.ValueKind == JsonValueKind.Array)
        {
            var arr = el.EnumerateArray().ToArray();
            var result = new byte[arr.Length];
            for (int i = 0; i < arr.Length; i++)
                result[i] = (byte)arr[i].GetInt32();
            return result;
        }
        return Array.Empty<byte>();
    }

    /// <summary>Convert a byte array to an int array for JSON serialization (matches serde format).</summary>
    private static int[] PayloadToJsonArray(byte[] data)
    {
        var arr = new int[data.Length];
        for (int i = 0; i < data.Length; i++)
            arr[i] = data[i] & 0xFF;
        return arr;
    }
}
