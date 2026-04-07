<?php

declare(strict_types=1);

namespace Aeon\ProcessorSdk\Tests;

use Aeon\ProcessorSdk\{BatchWire, Codec, Crc32, Event, Output, Signer};
use PHPUnit\Framework\TestCase;

class BatchWireTest extends TestCase
{
    public function testEncodeAndVerifySignedBatchResponse(): void
    {
        $codec = new Codec(Codec::JSON);
        $signer = Signer::generate();

        $outputsPerEvent = [
            [new Output(destination: 'out', payload: 'hello')],
            [],
        ];

        $response = BatchWire::encodeBatchResponse(42, $outputsPerEvent, $codec, $signer, true);

        $this->assertGreaterThan(12 + 4 + 64, strlen($response));
        $this->assertSame(42, BatchWire::readUint64LE($response, 0));
        $this->assertSame(2, BatchWire::readUint32LE($response, 8));

        $payloadEnd = strlen($response) - 64 - 4;
        $crcExpected = BatchWire::readUint32LE($response, $payloadEnd);
        $crcActual = Crc32::compute(substr($response, 0, $payloadEnd));
        $this->assertSame($crcExpected, $crcActual);

        $sigStart = strlen($response) - 64;
        $signedData = substr($response, 0, $sigStart);
        $sig = substr($response, $sigStart);
        $this->assertTrue($signer->verify($signedData, $sig));
    }

    public function testEncodeUnsignedBatchResponse(): void
    {
        $codec = new Codec(Codec::JSON);

        $response = BatchWire::encodeBatchResponse(
            1, [[new Output(destination: 'out', payload: 'x')]], $codec, null, false,
        );

        $sig = substr($response, -64);
        $this->assertSame(str_repeat("\x00", 64), $sig);
    }

    public function testRoundtripBatchRequest(): void
    {
        $codec = new Codec(Codec::JSON);

        $batchId = 99;
        $events = [
            new Event(id: 'abc-123', timestamp: 1000, source: 'src', partition: 0, metadata: [], payload: 'event-data'),
        ];

        $header = pack('P', $batchId) . pack('V', count($events));
        $eventParts = '';
        foreach ($events as $event) {
            $encoded = $codec->encodeEvent($event);
            $eventParts .= pack('V', strlen($encoded)) . $encoded;
        }
        $payload = $header . $eventParts;
        $request = $payload . pack('V', Crc32::compute($payload));

        $decoded = BatchWire::decodeBatchRequest($request, $codec);
        $this->assertSame($batchId, $decoded->batchId);
        $this->assertCount(1, $decoded->events);
        $this->assertSame('abc-123', $decoded->events[0]->id);
        $this->assertSame('event-data', $decoded->events[0]->payload);
    }

    public function testCrcMismatchThrows(): void
    {
        $codec = new Codec(Codec::JSON);

        $header = pack('P', 1) . pack('V', 0);
        $badCrc = pack('V', 0xDEADBEEF);
        $request = $header . $badCrc;

        $this->expectException(\RuntimeException::class);
        $this->expectExceptionMessageMatches('/CRC32 mismatch/');
        BatchWire::decodeBatchRequest($request, $codec);
    }

    public function testMultipleEventsInBatch(): void
    {
        $codec = new Codec(Codec::JSON);

        $batchId = 7;
        $events = [];
        for ($i = 0; $i < 5; $i++) {
            $events[] = new Event(
                id: "event-$i",
                timestamp: $i * 1000,
                source: 'src',
                partition: $i % 4,
                metadata: [['idx', (string)$i]],
                payload: "payload-$i",
                sourceOffset: $i,
            );
        }

        $header = pack('P', $batchId) . pack('V', count($events));
        $eventParts = '';
        foreach ($events as $event) {
            $encoded = $codec->encodeEvent($event);
            $eventParts .= pack('V', strlen($encoded)) . $encoded;
        }
        $payload = $header . $eventParts;
        $request = $payload . pack('V', Crc32::compute($payload));

        $decoded = BatchWire::decodeBatchRequest($request, $codec);
        $this->assertSame($batchId, $decoded->batchId);
        $this->assertCount(5, $decoded->events);
        for ($i = 0; $i < 5; $i++) {
            $this->assertSame("event-$i", $decoded->events[$i]->id);
            $this->assertSame($i % 4, $decoded->events[$i]->partition);
            $this->assertSame($i, $decoded->events[$i]->sourceOffset);
        }
    }
}
