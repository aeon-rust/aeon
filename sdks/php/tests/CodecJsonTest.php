<?php

declare(strict_types=1);

namespace Aeon\ProcessorSdk\Tests;

use Aeon\ProcessorSdk\{Codec, Event, Output};
use PHPUnit\Framework\TestCase;

class CodecJsonTest extends TestCase
{
    private Codec $codec;

    protected function setUp(): void
    {
        $this->codec = new Codec(Codec::JSON);
    }

    public function testRoundtripEvent(): void
    {
        $event = new Event(
            id: '550e8400-e29b-41d4-a716-446655440000',
            timestamp: 1712345678000000000,
            source: 'test-source',
            partition: 3,
            metadata: [['key1', 'val1']],
            payload: 'hello world',
            sourceOffset: 42,
        );

        $encoded = $this->codec->encodeEvent($event);
        $decoded = $this->codec->decodeEvent($encoded);

        $this->assertSame($event->id, $decoded->id);
        $this->assertSame($event->timestamp, $decoded->timestamp);
        $this->assertSame($event->source, $decoded->source);
        $this->assertSame($event->partition, $decoded->partition);
        $this->assertCount(1, $decoded->metadata);
        $this->assertSame('key1', $decoded->metadata[0][0]);
        $this->assertSame('val1', $decoded->metadata[0][1]);
        $this->assertSame($event->payload, $decoded->payload);
        $this->assertSame(42, $decoded->sourceOffset);
    }

    public function testRoundtripEventWithoutOptionalFields(): void
    {
        $event = new Event(
            id: '550e8400-e29b-41d4-a716-446655440000',
            timestamp: 0,
            source: 'src',
            partition: 0,
            metadata: [],
            payload: '',
            sourceOffset: null,
        );

        $encoded = $this->codec->encodeEvent($event);
        $decoded = $this->codec->decodeEvent($encoded);

        $this->assertSame($event->id, $decoded->id);
        $this->assertNull($decoded->sourceOffset);
    }

    public function testRoundtripOutput(): void
    {
        $output = new Output(
            destination: 'sink-topic',
            payload: 'output data',
            key: 'my-key',
            headers: [['h1', 'v1'], ['h2', 'v2']],
            sourceEventId: '550e8400-e29b-41d4-a716-446655440000',
            sourcePartition: 5,
            sourceOffset: 99,
        );

        $encoded = $this->codec->encodeOutput($output);
        $decoded = $this->codec->decodeOutput($encoded);

        $this->assertSame($output->destination, $decoded->destination);
        $this->assertSame($output->payload, $decoded->payload);
        $this->assertSame($output->key, $decoded->key);
        $this->assertCount(2, $decoded->headers);
        $this->assertSame($output->sourceEventId, $decoded->sourceEventId);
        $this->assertSame(5, $decoded->sourcePartition);
        $this->assertSame(99, $decoded->sourceOffset);
    }

    public function testRoundtripOutputWithoutKey(): void
    {
        $output = new Output(
            destination: 'out',
            payload: 'data',
        );

        $encoded = $this->codec->encodeOutput($output);
        $decoded = $this->codec->decodeOutput($encoded);

        $this->assertSame('out', $decoded->destination);
        $this->assertNull($decoded->key);
        $this->assertNull($decoded->sourceEventId);
    }

    public function testRoundtripEventBinaryPayload(): void
    {
        $event = new Event(
            id: 'test-binary',
            timestamp: 1000,
            source: 'bin',
            partition: 0,
            metadata: [],
            payload: "\x00\xFF\x55\xAA\x01\x02\x03",
        );

        $encoded = $this->codec->encodeEvent($event);
        $decoded = $this->codec->decodeEvent($encoded);
        $this->assertSame($event->payload, $decoded->payload);
    }
}
