// Aeon Processor SDK — WebSocket T4 runner with AWPP handshake.

using System.Net.WebSockets;
using System.Text;
using System.Text.Json;

namespace Aeon.ProcessorSdk;

/// <summary>Configuration for the WebSocket processor runner.</summary>
public sealed class RunnerConfig
{
    /// <summary>Processor name (registered with the engine).</summary>
    public string Name { get; set; } = "dotnet-processor";

    /// <summary>Processor version string.</summary>
    public string Version { get; set; } = "1.0.0";

    /// <summary>Path to 32-byte ED25519 seed file. Mutually exclusive with PrivateKey.</summary>
    public string? PrivateKeyPath { get; set; }

    /// <summary>32-byte ED25519 seed. Mutually exclusive with PrivateKeyPath.</summary>
    public byte[]? PrivateKey { get; set; }

    /// <summary>Requested pipeline names.</summary>
    public List<string> Pipelines { get; set; } = new();

    /// <summary>Transport codec: "msgpack" (default) or "json".</summary>
    public string CodecName { get; set; } = "msgpack";

    /// <summary>Processor registration (per-event or batch).</summary>
    public ProcessorRegistration Processor { get; set; } = ProcessorRegistration.PerEvent(_ => new List<Output>());

    /// <summary>Maximum batch size to request from the engine.</summary>
    public int MaxBatchSize { get; set; } = 1000;

    /// <summary>Binding mode: "dedicated" or "shared".</summary>
    public string Binding { get; set; } = "dedicated";
}

/// <summary>
/// Runs a processor connected to the Aeon engine via WebSocket (T4 transport).
/// Implements the full AWPP handshake: Challenge → Register → Accepted,
/// then enters the heartbeat + batch processing loop.
/// </summary>
public static class Runner
{
    /// <summary>
    /// Connect to the Aeon engine and run the processor loop until cancelled.
    /// </summary>
    /// <param name="url">WebSocket URL, e.g. ws://localhost:4471/api/v1/processors/connect</param>
    /// <param name="config">Runner configuration.</param>
    /// <param name="ct">Cancellation token for graceful shutdown.</param>
    public static async Task RunAsync(string url, RunnerConfig config, CancellationToken ct = default)
    {
        // Create signer
        using var signer = config.PrivateKeyPath != null
            ? Signer.FromFile(config.PrivateKeyPath)
            : config.PrivateKey != null
                ? Signer.FromSeed(config.PrivateKey)
                : Signer.Generate();

        var codec = new Codec(config.CodecName);

        using var ws = new ClientWebSocket();
        await ws.ConnectAsync(new Uri(url), ct);

        var recvBuf = new byte[4 * 1024 * 1024]; // 4MB receive buffer
        bool handshakeComplete = false;
        bool batchSigningEnabled = false;
        int heartbeatIntervalMs = 10000;

        // Heartbeat timer (started after handshake)
        Timer? heartbeatTimer = null;

        try
        {
            while (ws.State == WebSocketState.Open && !ct.IsCancellationRequested)
            {
                var result = await ws.ReceiveAsync(recvBuf, ct);

                if (result.MessageType == WebSocketMessageType.Close)
                    break;

                var msgData = new ReadOnlyMemory<byte>(recvBuf, 0, result.Count);

                if (!handshakeComplete || result.MessageType == WebSocketMessageType.Text)
                {
                    // Text frame — AWPP control message
                    var text = Encoding.UTF8.GetString(msgData.Span);
                    var msg = JsonDocument.Parse(text);
                    var msgType = msg.RootElement.GetProperty("type").GetString();

                    if (msgType == "challenge")
                    {
                        var nonce = msg.RootElement.GetProperty("nonce").GetString()!;
                        var reg = new Dictionary<string, object?>
                        {
                            ["type"] = "register",
                            ["protocol"] = "awpp/1",
                            ["transport"] = "websocket",
                            ["name"] = config.Name,
                            ["version"] = config.Version,
                            ["public_key"] = signer.PublicKeyFormatted,
                            ["challenge_signature"] = signer.SignChallenge(nonce),
                            ["oauth_token"] = null,
                            ["capabilities"] = new[] { "batch" },
                            ["max_batch_size"] = config.MaxBatchSize,
                            ["transport_codec"] = config.CodecName,
                            ["requested_pipelines"] = config.Pipelines.ToArray(),
                            ["binding"] = config.Binding,
                        };
                        var regJson = Encoding.UTF8.GetBytes(JsonSerializer.Serialize(reg));
                        await ws.SendAsync(regJson, WebSocketMessageType.Text, true, ct);
                    }
                    else if (msgType == "accepted")
                    {
                        handshakeComplete = true;
                        if (msg.RootElement.TryGetProperty("heartbeat_interval_ms", out var hb))
                            heartbeatIntervalMs = hb.GetInt32();
                        if (msg.RootElement.TryGetProperty("batch_signing", out var bs))
                            batchSigningEnabled = bs.GetBoolean();

                        // Start heartbeat
                        heartbeatTimer = new Timer(async _ =>
                        {
                            if (ws.State != WebSocketState.Open) return;
                            try
                            {
                                var hbMsg = JsonSerializer.Serialize(new Dictionary<string, object>
                                {
                                    ["type"] = "heartbeat",
                                    ["timestamp_ms"] = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds(),
                                });
                                var hbBytes = Encoding.UTF8.GetBytes(hbMsg);
                                await ws.SendAsync(hbBytes, WebSocketMessageType.Text, true, CancellationToken.None);
                            }
                            catch { /* heartbeat failure is non-fatal */ }
                        }, null, heartbeatIntervalMs, heartbeatIntervalMs);
                    }
                    else if (msgType == "rejected")
                    {
                        var code = msg.RootElement.TryGetProperty("code", out var c) ? c.GetString() : "unknown";
                        var message = msg.RootElement.TryGetProperty("message", out var m) ? m.GetString() : "";
                        throw new InvalidOperationException($"AWPP rejected: [{code}] {message}");
                    }
                    else if (msgType == "drain")
                    {
                        break;
                    }
                    continue;
                }

                // Binary frame — batch data with routing header
                var frame = DataFrameWire.Parse(msgData.Span);
                var batch = BatchWire.DecodeBatchRequest(frame.BatchData, codec);

                // Process events
                List<List<Output>> outputsPerEvent;
                if (config.Processor.BatchProcessFn != null)
                {
                    outputsPerEvent = config.Processor.BatchProcessFn(batch.Events);
                }
                else if (config.Processor.ProcessFn != null)
                {
                    outputsPerEvent = batch.Events
                        .Select(e => config.Processor.ProcessFn(e))
                        .ToList();
                }
                else
                {
                    outputsPerEvent = batch.Events.Select(_ => new List<Output>()).ToList();
                }

                // Encode and send response
                var responseBatch = BatchWire.EncodeBatchResponse(
                    batch.BatchId, outputsPerEvent, codec, signer, batchSigningEnabled);
                var responseFrame = DataFrameWire.Build(
                    frame.PipelineName, frame.Partition, responseBatch);
                await ws.SendAsync(responseFrame, WebSocketMessageType.Binary, true, ct);
            }
        }
        finally
        {
            heartbeatTimer?.Dispose();
            if (ws.State == WebSocketState.Open)
            {
                try { await ws.CloseAsync(WebSocketCloseStatus.NormalClosure, "shutdown", CancellationToken.None); }
                catch { /* best-effort close */ }
            }
        }
    }
}
