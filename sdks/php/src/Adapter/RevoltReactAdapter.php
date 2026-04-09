<?php

declare(strict_types=1);

namespace Aeon\ProcessorSdk\Adapter;

use Aeon\ProcessorSdk\{BatchWire, Codec, DataFrameWire, Processor, Signer};

/**
 * RevoltPHP + ReactPHP adapter — Revolt event loop with Ratchet WebSocket client.
 *
 * Requires:
 *   - revolt/event-loop (Fiber-based event loop, PHP 8.1+)
 *   - ratchet/pawl (ReactPHP WebSocket client)
 *   - react/socket (ReactPHP socket)
 *
 * RevoltPHP provides the modern async foundation; ReactPHP ecosystem
 * (specifically Ratchet/Pawl) provides the WebSocket client implementation.
 */
final class RevoltReactAdapter implements AdapterInterface
{
    public function run(string $url, array $config, Processor $processor): void
    {
        if (!class_exists(\Ratchet\Client\Connector::class)) {
            throw new \RuntimeException('ratchet/pawl is required for RevoltReactAdapter (composer require ratchet/pawl)');
        }

        $loop = \React\EventLoop\Loop::get();

        $signer = AwppHelper::createSigner($config);
        $codec = new Codec($config['codec'] ?? 'msgpack');
        $handshakeComplete = false;
        $batchSigningEnabled = false;
        $heartbeatIntervalMs = 10000;

        $connector = new \Ratchet\Client\Connector($loop);
        $connector($url)->then(
            function (\Ratchet\Client\WebSocket $conn) use (
                $config, $processor, $signer, $codec,
                &$handshakeComplete, &$batchSigningEnabled, &$heartbeatIntervalMs, $loop,
            ) {
                $conn->on('message', function ($msg) use (
                    $conn, $config, $processor, $signer, $codec,
                    &$handshakeComplete, &$batchSigningEnabled, &$heartbeatIntervalMs, $loop,
                ) {
                    $data = (string)$msg;
                    $isBinary = $msg instanceof \Ratchet\RFC6455\Messaging\Frame
                        && $msg->getOpcode() === 2;

                    if (!$handshakeComplete || !$isBinary) {
                        $result = AwppHelper::handleControlMessage(
                            $data, $signer, $config, $codec,
                            $handshakeComplete, $batchSigningEnabled, $heartbeatIntervalMs,
                        );
                        if ($result !== null) {
                            $conn->send($result);
                        }
                        if ($handshakeComplete) {
                            // Start heartbeat timer
                            $loop->addPeriodicTimer($heartbeatIntervalMs / 1000.0, function () use ($conn) {
                                $conn->send(json_encode([
                                    'type' => 'heartbeat',
                                    'timestamp_ms' => (int)(microtime(true) * 1000),
                                ]));
                            });
                        }
                        return;
                    }

                    $response = AwppHelper::processBatchFrame(
                        $data, $codec, $signer, $batchSigningEnabled, $processor,
                    );
                    $conn->send(new \Ratchet\RFC6455\Messaging\Frame($response, true, 2));
                });

                $conn->on('close', function () use ($loop) {
                    $loop->stop();
                });
            },
            function (\Exception $e) use ($loop) {
                throw new \RuntimeException("WebSocket connection failed: {$e->getMessage()}", 0, $e);
            }
        );

        $loop->run();
    }
}
