<?php

declare(strict_types=1);

namespace Aeon\ProcessorSdk\Tests;

use Aeon\ProcessorSdk\{BatchWire, Codec, Crc32, Output, Signer};
use PHPUnit\Framework\TestCase;

class CrossCodecTest extends TestCase
{
    public function testBatchWireFormatValidWithJsonCodec(): void
    {
        $codec = new Codec(Codec::JSON);
        $signer = Signer::generate();

        $outputs = [
            [new Output(
                destination: 'topic-out',
                payload: 'binary-data',
                key: 'key-1',
                headers: [['content-type', 'application/octet-stream']],
            )],
        ];

        $response = BatchWire::encodeBatchResponse(1, $outputs, $codec, $signer, true);

        $this->assertSame(1, BatchWire::readUint64LE($response, 0));
        $this->assertSame(1, BatchWire::readUint32LE($response, 8));

        $crcOffset = strlen($response) - 64 - 4;
        $expectedCrc = BatchWire::readUint32LE($response, $crcOffset);
        $actualCrc = Crc32::compute(substr($response, 0, $crcOffset));
        $this->assertSame($expectedCrc, $actualCrc);

        $signedData = substr($response, 0, strlen($response) - 64);
        $sig = substr($response, -64);
        $this->assertTrue($signer->verify($signedData, $sig));
    }
}
