<?php

declare(strict_types=1);

namespace Aeon\ProcessorSdk\Adapter;

use Aeon\ProcessorSdk\{BatchWire, Codec, DataFrameWire, Processor, Signer};

/**
 * Native CLI adapter — blocking stream_socket_client + poll-based WebSocket.
 *
 * Works on any PHP 8.1+ without extensions. Lowest performance but universal
 * compatibility. Uses stream_socket_client for TCP/TLS connection and
 * implements a minimal WebSocket client (RFC 6455) with blocking I/O.
 *
 * This is the fallback adapter when no async runtime is available.
 */
final class NativeCliAdapter implements AdapterInterface
{
    /** @var resource|null */
    private $socket = null;

    public function run(string $url, array $config, Processor $processor): void
    {
        $signer = AwppHelper::createSigner($config);
        $codec = new Codec($config['codec'] ?? 'msgpack');
        $handshakeComplete = false;
        $batchSigningEnabled = false;
        $heartbeatIntervalMs = 10000;

        $this->connect($url);
        $lastHeartbeat = time();

        try {
            while (true) {
                // Check for heartbeat
                if ($handshakeComplete && (time() - $lastHeartbeat) * 1000 >= $heartbeatIntervalMs) {
                    $this->sendText(json_encode([
                        'type' => 'heartbeat',
                        'timestamp_ms' => (int)(microtime(true) * 1000),
                    ]));
                    $lastHeartbeat = time();
                }

                // Read frame with timeout
                $frame = $this->readFrame(5.0);
                if ($frame === null) {
                    continue; // timeout, try again (heartbeat will fire)
                }

                if ($frame['opcode'] === 0x08) {
                    break; // Close frame
                }

                if ($frame['opcode'] === 0x09) {
                    // Ping → Pong
                    $this->sendFrame(0x0A, $frame['data']);
                    continue;
                }

                $data = $frame['data'];

                if (!$handshakeComplete || $frame['opcode'] === 0x01) {
                    // Text frame — control message
                    $result = AwppHelper::handleControlMessage(
                        $data, $signer, $config, $codec,
                        $handshakeComplete, $batchSigningEnabled, $heartbeatIntervalMs,
                    );
                    if ($result !== null) {
                        $this->sendText($result);
                    }
                    continue;
                }

                // Binary frame — batch data
                $response = AwppHelper::processBatchFrame(
                    $data, $codec, $signer, $batchSigningEnabled, $processor,
                );
                $this->sendBinary($response);
            }
        } finally {
            $this->close();
        }
    }

    // ── Minimal WebSocket client (RFC 6455) ──────────────────────────

    private function connect(string $url): void
    {
        $parsed = parse_url($url);
        $scheme = $parsed['scheme'] ?? 'ws';
        $host = $parsed['host'] ?? 'localhost';
        $port = $parsed['port'] ?? ($scheme === 'wss' ? 443 : 80);
        $path = $parsed['path'] ?? '/api/v1/processors/connect';

        $transport = ($scheme === 'wss') ? 'ssl' : 'tcp';
        $address = "$transport://$host:$port";

        $context = stream_context_create();
        $this->socket = @stream_socket_client($address, $errno, $errstr, 30, STREAM_CLIENT_CONNECT, $context);
        if ($this->socket === false) {
            throw new \RuntimeException("WebSocket connect failed: [$errno] $errstr");
        }

        // WebSocket HTTP upgrade handshake
        $key = base64_encode(random_bytes(16));
        $request = "GET $path HTTP/1.1\r\n"
            . "Host: $host:$port\r\n"
            . "Upgrade: websocket\r\n"
            . "Connection: Upgrade\r\n"
            . "Sec-WebSocket-Key: $key\r\n"
            . "Sec-WebSocket-Version: 13\r\n"
            . "\r\n";

        fwrite($this->socket, $request);

        // Read HTTP response (blocking)
        $response = '';
        while (!str_contains($response, "\r\n\r\n")) {
            $chunk = fread($this->socket, 4096);
            if ($chunk === false || $chunk === '') {
                throw new \RuntimeException('WebSocket handshake failed: no response');
            }
            $response .= $chunk;
        }

        if (!str_contains($response, '101')) {
            throw new \RuntimeException("WebSocket upgrade failed: $response");
        }
    }

    /**
     * Read a WebSocket frame.
     * @return array{opcode: int, data: string}|null Null on timeout.
     */
    private function readFrame(float $timeoutSec): ?array
    {
        stream_set_timeout($this->socket, (int)$timeoutSec, (int)(($timeoutSec - (int)$timeoutSec) * 1000000));

        $header = @fread($this->socket, 2);
        if ($header === false || strlen($header) < 2) {
            $info = stream_get_meta_data($this->socket);
            if ($info['timed_out']) {
                return null;
            }
            throw new \RuntimeException('WebSocket read error');
        }

        $byte1 = ord($header[0]);
        $byte2 = ord($header[1]);
        $opcode = $byte1 & 0x0F;
        $masked = ($byte2 & 0x80) !== 0;
        $payloadLen = $byte2 & 0x7F;

        if ($payloadLen === 126) {
            $ext = $this->readExact(2);
            $payloadLen = unpack('n', $ext)[1];
        } elseif ($payloadLen === 127) {
            $ext = $this->readExact(8);
            $payloadLen = unpack('J', $ext)[1];
        }

        $maskKey = $masked ? $this->readExact(4) : '';
        $payload = $payloadLen > 0 ? $this->readExact($payloadLen) : '';

        if ($masked && $maskKey !== '') {
            for ($i = 0; $i < $payloadLen; $i++) {
                $payload[$i] = chr(ord($payload[$i]) ^ ord($maskKey[$i % 4]));
            }
        }

        return ['opcode' => $opcode, 'data' => $payload];
    }

    private function readExact(int $length): string
    {
        $data = '';
        while (strlen($data) < $length) {
            $chunk = fread($this->socket, $length - strlen($data));
            if ($chunk === false || $chunk === '') {
                throw new \RuntimeException('WebSocket read error: unexpected EOF');
            }
            $data .= $chunk;
        }
        return $data;
    }

    private function sendFrame(int $opcode, string $data): void
    {
        $len = strlen($data);
        $frame = chr(0x80 | $opcode); // FIN + opcode

        // Client frames MUST be masked (RFC 6455)
        $mask = random_bytes(4);

        if ($len < 126) {
            $frame .= chr(0x80 | $len);
        } elseif ($len < 65536) {
            $frame .= chr(0x80 | 126) . pack('n', $len);
        } else {
            $frame .= chr(0x80 | 127) . pack('J', $len);
        }

        $frame .= $mask;

        // Mask the payload
        $masked = '';
        for ($i = 0; $i < $len; $i++) {
            $masked .= chr(ord($data[$i]) ^ ord($mask[$i % 4]));
        }
        $frame .= $masked;

        fwrite($this->socket, $frame);
    }

    private function sendText(string $data): void
    {
        $this->sendFrame(0x01, $data);
    }

    private function sendBinary(string $data): void
    {
        $this->sendFrame(0x02, $data);
    }

    private function close(): void
    {
        if ($this->socket !== null) {
            try {
                $this->sendFrame(0x08, '');
            } catch (\Throwable) {
                // best-effort close
            }
            fclose($this->socket);
            $this->socket = null;
        }
    }
}
