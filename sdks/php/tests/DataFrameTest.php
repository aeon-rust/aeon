<?php

declare(strict_types=1);

namespace Aeon\ProcessorSdk\Tests;

use Aeon\ProcessorSdk\DataFrameWire;
use PHPUnit\Framework\TestCase;

class DataFrameTest extends TestCase
{
    public function testRoundtripDataFrame(): void
    {
        $name = 'my-pipeline';
        $partition = 7;
        $batchData = 'batch-content';

        $frame = DataFrameWire::build($name, $partition, $batchData);
        $parsed = DataFrameWire::parse($frame);

        $this->assertSame($name, $parsed->pipelineName);
        $this->assertSame($partition, $parsed->partition);
        $this->assertSame($batchData, $parsed->batchData);
    }

    public function testEmptyPipelineName(): void
    {
        $frame = DataFrameWire::build('', 0, 'data');
        $parsed = DataFrameWire::parse($frame);
        $this->assertSame('', $parsed->pipelineName);
        $this->assertSame(0, $parsed->partition);
    }

    public function testUnicodePipelineName(): void
    {
        $name = "pipeline-\u{65E5}\u{672C}\u{8A9E}";
        $frame = DataFrameWire::build($name, 100, 'x');
        $parsed = DataFrameWire::parse($frame);
        $this->assertSame($name, $parsed->pipelineName);
    }

    public function testTooShortFrameThrows(): void
    {
        $this->expectException(\RuntimeException::class);
        $this->expectExceptionMessageMatches('/too short/');
        DataFrameWire::parse("\x00\x00\x00");
    }
}
