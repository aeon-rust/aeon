<?php

declare(strict_types=1);

namespace Aeon\ProcessorSdk\Adapter;

use Aeon\ProcessorSdk\{BatchWire, Codec, DataFrameWire, Processor, Signer};

/**
 * Workerman adapter — built-in event-driven WebSocket client.
 *
 * Requires: workerman/workerman (^4.0)
 *
 * Workerman is a standalone event-driven framework with built-in WebSocket
 * client support. No ext-event dependency required. Popular in the China/Asia
 * PHP community for high-performance network applications.
 */
final class WorkermanAdapter implements AdapterInterface
{
    public function run(string $url, array $config, Processor $processor): void
    {
        if (!class_exists(\Workerman\Worker::class)) {
            throw new \RuntimeException('workerman/workerman is required for WorkermanAdapter (composer require workerman/workerman)');
        }

        $signer = AwppHelper::createSigner($config);
        $codec = new Codec($config['codec'] ?? 'msgpack');
        $handshakeComplete = false;
        $batchSigningEnabled = false;
        $heartbeatIntervalMs = 10000;

        $worker = new \Workerman\Worker();
        $worker->onWorkerStart = function () use (
            $url, $config, $processor, $signer, $codec,
            &$handshakeComplete, &$batchSigningEnabled, &$heartbeatIntervalMs,
        ) {
            $con = new \Workerman\Protocols\Websocket\Client($url);

            $con->onConnect = function ($connection) {};

            $con->onMessage = function ($connection, $data) use (
                $config, $processor, $signer, $codec,
                &$handshakeComplete, &$batchSigningEnabled, &$heartbeatIntervalMs,
            ) {
                // Workerman delivers text/binary messages through onMessage
                if (!$handshakeComplete) {
                    $result = AwppHelper::handleControlMessage(
                        $data, $signer, $config, $codec,
                        $handshakeComplete, $batchSigningEnabled, $heartbeatIntervalMs,
                    );
                    if ($result !== null) {
                        $connection->send($result);
                    }
                    if ($handshakeComplete) {
                        // Start heartbeat timer
                        \Workerman\Timer::add($heartbeatIntervalMs / 1000.0, function () use ($connection) {
                            $connection->send(json_encode([
                                'type' => 'heartbeat',
                                'timestamp_ms' => (int)(microtime(true) * 1000),
                            ]));
                        });
                    }
                    return;
                }

                // Check if binary (batch) or text (control)
                $decoded = @json_decode($data, true);
                if ($decoded !== null && isset($decoded['type'])) {
                    $result = AwppHelper::handleControlMessage(
                        $data, $signer, $config, $codec,
                        $handshakeComplete, $batchSigningEnabled, $heartbeatIntervalMs,
                    );
                    if ($result !== null) {
                        $connection->send($result);
                    }
                    return;
                }

                // Binary batch data
                $response = AwppHelper::processBatchFrame(
                    $data, $codec, $signer, $batchSigningEnabled, $processor,
                );
                $connection->send($response, true); // true = binary
            };

            $con->onClose = function () {
                \Workerman\Worker::stopAll();
            };

            $con->connect();
        };

        \Workerman\Worker::runAll();
    }
}
