<?php

declare(strict_types=1);

namespace Aeon\ProcessorSdk\Tests;

use Aeon\ProcessorSdk\Crc32;
use PHPUnit\Framework\TestCase;

class Crc32Test extends TestCase
{
    public function testEmptyBuffer(): void
    {
        $this->assertSame(0, Crc32::compute(''));
    }

    public function testKnownValue123456789(): void
    {
        $this->assertSame(0xCBF43926, Crc32::compute('123456789'));
    }

    public function testBinaryData(): void
    {
        $data = "\x00\xFF\x55\xAA";
        $result = Crc32::compute($data);
        $this->assertIsInt($result);
        $this->assertGreaterThan(0, $result);
        $this->assertSame($result, Crc32::compute($data));
    }
}
