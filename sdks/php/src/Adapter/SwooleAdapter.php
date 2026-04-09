<?php

declare(strict_types=1);

namespace Aeon\ProcessorSdk\Adapter;

use Aeon\ProcessorSdk\{BatchWire, Codec, DataFrameWire, Processor, Signer};

/**
 * Swoole / OpenSwoole adapter — coroutine WebSocket client.
 *
 * Requires: ext-swoole or ext-openswoole
 * Also works with Laravel Octane (Swoole driver).
 *
 * Uses Swoole's native coroutine scheduler for async I/O, providing
 * high-performance non-blocking WebSocket communication.
 */
final class SwooleAdapter implements AdapterInterface
{
    public function run(string $url, array $config, Processor $processor): void
    {
        if (!extension_loaded('swoole') && !extension_loaded('openswoole')) {
            throw new \RuntimeException('ext-swoole or ext-openswoole is required for SwooleAdapter');
        }

        \Swoole\Coroutine\run(function () use ($url, $config, $processor) {
            $this->connectAndProcess($url, $config, $processor);
        });
    }

    private function connectAndProcess(string $url, array $config, Processor $processor): void
    {
        $parsed = parse_url($url);
        $host = $parsed['host'] ?? 'localhost';
        $port = $parsed['port'] ?? ($parsed['scheme'] === 'wss' ? 443 : 80);
        $path = $parsed['path'] ?? '/api/v1/processors/connect';
        $ssl = ($parsed['scheme'] ?? '') === 'wss';

        $client = new \Swoole\Coroutine\Http\Client($host, $port, $ssl);
        $client->upgrade($path);

        $signer = AwppHelper::createSigner($config);
        $codec = new Codec($config['codec'] ?? 'msgpack');
        $handshakeComplete = false;
        $batchSigningEnabled = false;
        $heartbeatIntervalMs = 10000;

        while (true) {
            $frame = $client->recv(30.0);
            if ($frame === false || $frame === '') {
                break;
            }

            if (!$handshakeComplete || $frame->opcode === WEBSOCKET_OPCODE_TEXT) {
                $result = AwppHelper::handleControlMessage(
                    $frame->data, $signer, $config, $codec,
                    $handshakeComplete, $batchSigningEnabled, $heartbeatIntervalMs,
                );
                if ($result !== null) {
                    $client->push($result, WEBSOCKET_OPCODE_TEXT);
                }
                if ($result === null && !$handshakeComplete) {
                    break; // rejected or drain
                }
                continue;
            }

            // Binary frame — batch data
            $response = AwppHelper::processBatchFrame(
                $frame->data, $codec, $signer, $batchSigningEnabled, $processor,
            );
            $client->push($response, WEBSOCKET_OPCODE_BINARY);
        }

        $client->close();
    }
}
