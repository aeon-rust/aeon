<?php

declare(strict_types=1);

namespace Aeon\ProcessorSdk\Adapter;

use Aeon\ProcessorSdk\{BatchWire, Codec, DataFrameWire, Processor, Signer};

/**
 * RevoltPHP + AMPHP adapter — Revolt event loop with AMPHP WebSocket client.
 *
 * Requires:
 *   - revolt/event-loop (Fiber-based event loop, PHP 8.1+)
 *   - amphp/websocket-client (AMPHP WebSocket client)
 *
 * Fiber-native, modern PHP 8.1+ async. AMPHP provides first-class
 * WebSocket support built directly on Revolt event loop.
 */
final class RevoltAmphpAdapter implements AdapterInterface
{
    public function run(string $url, array $config, Processor $processor): void
    {
        if (!class_exists(\Amp\Websocket\Client\WebsocketHandshake::class)) {
            throw new \RuntimeException('amphp/websocket-client is required for RevoltAmphpAdapter (composer require amphp/websocket-client)');
        }

        $signer = AwppHelper::createSigner($config);
        $codec = new Codec($config['codec'] ?? 'msgpack');
        $handshakeComplete = false;
        $batchSigningEnabled = false;
        $heartbeatIntervalMs = 10000;

        $handshake = new \Amp\Websocket\Client\WebsocketHandshake($url);
        $connection = \Amp\Websocket\Client\connect($handshake);

        // Start heartbeat in background after handshake
        $heartbeatId = null;

        foreach ($connection as $message) {
            $data = $message->buffer();
            $isBinary = $message->isBinary();

            if (!$handshakeComplete || !$isBinary) {
                $result = AwppHelper::handleControlMessage(
                    $data, $signer, $config, $codec,
                    $handshakeComplete, $batchSigningEnabled, $heartbeatIntervalMs,
                );
                if ($result !== null) {
                    $connection->sendText($result);
                }
                if ($handshakeComplete && $heartbeatId === null) {
                    // Start heartbeat using Revolt event loop
                    $heartbeatId = \Revolt\EventLoop::repeat(
                        $heartbeatIntervalMs / 1000.0,
                        function () use ($connection) {
                            $connection->sendText(json_encode([
                                'type' => 'heartbeat',
                                'timestamp_ms' => (int)(microtime(true) * 1000),
                            ]));
                        }
                    );
                }
                continue;
            }

            $response = AwppHelper::processBatchFrame(
                $data, $codec, $signer, $batchSigningEnabled, $processor,
            );
            $connection->sendBinary($response);
        }

        if ($heartbeatId !== null) {
            \Revolt\EventLoop::cancel($heartbeatId);
        }
        $connection->close();
    }
}
