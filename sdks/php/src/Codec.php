<?php

declare(strict_types=1);

namespace Aeon\ProcessorSdk;

/**
 * MsgPack + JSON codec for Event/Output serialization.
 *
 * MsgPack requires ext-msgpack. Falls back to JSON if not available.
 * Field names match the AWPP protocol wire format.
 */
final class Codec
{
    public const MSGPACK = 'msgpack';
    public const JSON = 'json';

    public readonly string $name;
    private readonly bool $isMsgpack;

    public function __construct(string $name = self::MSGPACK)
    {
        if ($name === self::MSGPACK && !extension_loaded('msgpack')) {
            // Fallback to JSON if msgpack extension not available
            $name = self::JSON;
        }
        $this->name = $name;
        $this->isMsgpack = ($name === self::MSGPACK);
    }

    // ── Event ────────────────────────────────────────────────────────

    public function encodeEvent(Event $event): string
    {
        $data = [
            'id' => $event->id,
            'timestamp' => $event->timestamp,
            'source' => $event->source,
            'partition' => $event->partition,
            'metadata' => $event->metadata,
            'payload' => $this->isMsgpack ? $event->payload : base64_encode($event->payload),
        ];
        if ($event->sourceOffset !== null) {
            $data['source_offset'] = $event->sourceOffset;
        }
        return $this->pack($data);
    }

    public function decodeEvent(string $data): Event
    {
        $obj = $this->unpack($data);
        $payload = $this->isMsgpack
            ? (string)($obj['payload'] ?? '')
            : base64_decode((string)($obj['payload'] ?? ''));

        return new Event(
            id: (string)($obj['id'] ?? ''),
            timestamp: (int)($obj['timestamp'] ?? 0),
            source: (string)($obj['source'] ?? ''),
            partition: (int)($obj['partition'] ?? 0),
            metadata: self::toMetadata($obj['metadata'] ?? []),
            payload: $payload,
            sourceOffset: isset($obj['source_offset']) ? (int)$obj['source_offset'] : null,
        );
    }

    // ── Output ───────────────────────────────────────────────────────

    public function encodeOutput(Output $output): string
    {
        $data = [
            'destination' => $output->destination,
            'payload' => $this->isMsgpack ? $output->payload : base64_encode($output->payload),
            'headers' => $output->headers,
        ];
        if ($output->key !== null) {
            $data['key'] = $this->isMsgpack ? $output->key : base64_encode($output->key);
        }
        if ($output->sourceEventId !== null) {
            $data['source_event_id'] = $output->sourceEventId;
        }
        if ($output->sourcePartition !== null) {
            $data['source_partition'] = $output->sourcePartition;
        }
        if ($output->sourceOffset !== null) {
            $data['source_offset'] = $output->sourceOffset;
        }
        return $this->pack($data);
    }

    public function decodeOutput(string $data): Output
    {
        $obj = $this->unpack($data);
        $payload = $this->isMsgpack
            ? (string)($obj['payload'] ?? '')
            : base64_decode((string)($obj['payload'] ?? ''));
        $key = null;
        if (isset($obj['key'])) {
            $key = $this->isMsgpack
                ? (string)$obj['key']
                : base64_decode((string)$obj['key']);
        }

        return new Output(
            destination: (string)($obj['destination'] ?? ''),
            payload: $payload,
            key: $key,
            headers: self::toMetadata($obj['headers'] ?? []),
            sourceEventId: isset($obj['source_event_id']) ? (string)$obj['source_event_id'] : null,
            sourcePartition: isset($obj['source_partition']) ? (int)$obj['source_partition'] : null,
            sourceOffset: isset($obj['source_offset']) ? (int)$obj['source_offset'] : null,
        );
    }

    // ── Internal ─────────────────────────────────────────────────────

    private function pack(array $data): string
    {
        if ($this->isMsgpack) {
            return \msgpack_pack($data);
        }
        return json_encode($data, JSON_THROW_ON_ERROR | JSON_UNESCAPED_SLASHES);
    }

    private function unpack(string $data): array
    {
        if ($this->isMsgpack) {
            $result = \msgpack_unpack($data);
            return is_array($result) ? $result : [];
        }
        return json_decode($data, true, 512, JSON_THROW_ON_ERROR);
    }

    /**
     * @return list<array{0: string, 1: string}>
     */
    private static function toMetadata(mixed $raw): array
    {
        if (!is_array($raw)) {
            return [];
        }
        $result = [];
        foreach ($raw as $pair) {
            if (is_array($pair) && count($pair) >= 2) {
                $result[] = [(string)$pair[0], (string)$pair[1]];
            }
        }
        return $result;
    }
}
