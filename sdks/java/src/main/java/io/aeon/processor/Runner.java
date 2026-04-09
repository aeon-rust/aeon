package io.aeon.processor;

import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.WebSocket;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionStage;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicReference;

/**
 * AWPP WebSocket runner (T4 transport).
 * Handles the full AWPP handshake, heartbeat loop, and binary frame processing.
 */
public final class Runner {

    private Runner() {}

    /**
     * Runner configuration.
     */
    public record RunnerConfig(
        String name,
        String version,
        byte[] privateKey,
        List<String> pipelines,
        String codec,
        Processor processor,
        BatchProcessor batchProcessor
    ) {
        public RunnerConfig {
            if (name == null || name.isEmpty()) throw new IllegalArgumentException("name required");
            if (version == null || version.isEmpty()) throw new IllegalArgumentException("version required");
            if (pipelines == null || pipelines.isEmpty()) throw new IllegalArgumentException("pipelines required");
            if (processor == null && batchProcessor == null)
                throw new IllegalArgumentException("processor or batchProcessor required");
            if (codec == null || codec.isEmpty()) codec = "json";
        }
    }

    /**
     * Connect to an Aeon engine via WebSocket and run the AWPP protocol.
     * Blocks until the connection is closed or an error occurs.
     */
    public static void run(String url, RunnerConfig config) {
        var signer = config.privateKey() != null
                ? Signer.fromSeed(config.privateKey())
                : Signer.generate();
        var codecInst = new Codec();
        var closeLatch = new CountDownLatch(1);
        var batchSigning = new AtomicBoolean(false);
        var heartbeatInterval = new AtomicReference<Long>(10000L);
        var sessionEstablished = new AtomicBoolean(false);
        var wsRef = new AtomicReference<WebSocket>();
        var textBuffer = new StringBuilder();

        var listener = new WebSocket.Listener() {

            @Override
            public CompletionStage<?> onText(WebSocket webSocket, CharSequence data, boolean last) {
                textBuffer.append(data);
                if (!last) {
                    webSocket.request(1);
                    return CompletableFuture.completedFuture(null);
                }

                String message = textBuffer.toString();
                textBuffer.setLength(0);
                webSocket.request(1);

                try {
                    var json = Codec.parseObject(message);
                    String type = (String) json.get("type");

                    switch (type) {
                        case "challenge" -> handleChallenge(webSocket, json, config, signer);
                        case "accepted" -> {
                            String sessionId = (String) json.get("session_id");
                            Object hbInterval = json.get("heartbeat_interval_ms");
                            Object bs = json.get("batch_signing");
                            if (hbInterval != null) {
                                heartbeatInterval.set(((Number) hbInterval).longValue());
                            }
                            if (bs != null && (bs.equals(Boolean.TRUE) || "true".equals(bs.toString()))) {
                                batchSigning.set(true);
                            }
                            sessionEstablished.set(true);
                            System.out.println("[AEON] Session established: " + sessionId);
                            // Start heartbeat thread
                            startHeartbeat(webSocket, heartbeatInterval.get(), closeLatch);
                        }
                        case "heartbeat" -> { /* Server heartbeat — acknowledged implicitly */ }
                        case "error" -> {
                            System.err.println("[AEON] Error from server: " + json.get("message"));
                            closeLatch.countDown();
                        }
                        default -> System.err.println("[AEON] Unknown message type: " + type);
                    }
                } catch (Exception e) {
                    System.err.println("[AEON] Error handling text message: " + e.getMessage());
                }

                return CompletableFuture.completedFuture(null);
            }

            @Override
            public CompletionStage<?> onBinary(WebSocket webSocket, ByteBuffer data, boolean last) {
                webSocket.request(1);

                try {
                    byte[] frameBytes = new byte[data.remaining()];
                    data.get(frameBytes);

                    // Parse data frame (routing header)
                    var frame = Wire.parseDataFrame(frameBytes);

                    // Decode batch request
                    var batchReq = Wire.decodeBatchRequest(frame.batchData(), codecInst);

                    // Process events
                    List<List<Output>> outputsPerEvent;
                    if (config.batchProcessor() != null) {
                        outputsPerEvent = config.batchProcessor().processBatch(batchReq.events());
                    } else {
                        outputsPerEvent = new ArrayList<>();
                        for (var event : batchReq.events()) {
                            outputsPerEvent.add(config.processor().process(event));
                        }
                    }

                    // Encode batch response
                    byte[] responseData = Wire.encodeBatchResponse(
                            batchReq.batchId(),
                            outputsPerEvent,
                            codecInst,
                            signer,
                            batchSigning.get()
                    );

                    // Wrap in data frame
                    byte[] responseFrame = Wire.buildDataFrame(
                            frame.pipelineName(),
                            frame.partition(),
                            responseData
                    );

                    // Send binary response
                    webSocket.sendBinary(ByteBuffer.wrap(responseFrame), true);

                } catch (Exception e) {
                    System.err.println("[AEON] Error processing binary frame: " + e.getMessage());
                    e.printStackTrace();
                }

                return CompletableFuture.completedFuture(null);
            }

            @Override
            public CompletionStage<?> onClose(WebSocket webSocket, int statusCode, String reason) {
                System.out.println("[AEON] Connection closed: " + statusCode + " " + reason);
                closeLatch.countDown();
                return CompletableFuture.completedFuture(null);
            }

            @Override
            public void onError(WebSocket webSocket, Throwable error) {
                System.err.println("[AEON] WebSocket error: " + error.getMessage());
                closeLatch.countDown();
            }
        };

        try {
            var client = HttpClient.newHttpClient();
            var ws = client.newWebSocketBuilder()
                    .subprotocols("awpp")
                    .buildAsync(URI.create(url), listener)
                    .join();
            wsRef.set(ws);

            System.out.println("[AEON] Connected to " + url);
            ws.request(1);

            // Block until connection closes
            closeLatch.await();
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            System.err.println("[AEON] Interrupted");
        } catch (Exception e) {
            System.err.println("[AEON] Connection failed: " + e.getMessage());
        }
    }

    private static void handleChallenge(
            WebSocket ws,
            Map<String, Object> challenge,
            RunnerConfig config,
            Signer signer
    ) {
        String nonce = (String) challenge.get("nonce");
        String signature = signer.signChallenge(nonce);

        // Build register message
        var sb = new StringBuilder(512);
        sb.append("{\"type\":\"register\"");
        sb.append(",\"protocol\":\"awpp/1\"");
        sb.append(",\"transport\":\"websocket\"");
        sb.append(",\"name\":\"").append(jsonEscape(config.name())).append('"');
        sb.append(",\"version\":\"").append(jsonEscape(config.version())).append('"');
        sb.append(",\"public_key\":\"").append(jsonEscape(signer.publicKeyFormatted())).append('"');
        sb.append(",\"challenge_signature\":\"").append(signature).append('"');
        sb.append(",\"capabilities\":[\"batch\"]");
        sb.append(",\"max_batch_size\":1000");
        sb.append(",\"transport_codec\":\"").append(jsonEscape(config.codec())).append('"');
        sb.append(",\"requested_pipelines\":[");
        for (int i = 0; i < config.pipelines().size(); i++) {
            if (i > 0) sb.append(',');
            sb.append('"').append(jsonEscape(config.pipelines().get(i))).append('"');
        }
        sb.append(']');
        sb.append(",\"binding\":\"dedicated\"");
        sb.append('}');

        ws.sendText(sb.toString(), true);
    }

    private static void startHeartbeat(WebSocket ws, long intervalMs, CountDownLatch closeLatch) {
        var thread = new Thread(() -> {
            try {
                while (closeLatch.getCount() > 0) {
                    Thread.sleep(intervalMs);
                    if (closeLatch.getCount() == 0) break;
                    long ts = System.currentTimeMillis();
                    String hb = "{\"type\":\"heartbeat\",\"timestamp_ms\":" + ts + "}";
                    ws.sendText(hb, true);
                }
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            } catch (Exception e) {
                // Connection likely closed
            }
        }, "aeon-heartbeat");
        thread.setDaemon(true);
        thread.start();
    }

    private static String jsonEscape(String s) {
        if (s == null) return "";
        return s.replace("\\", "\\\\").replace("\"", "\\\"");
    }
}
