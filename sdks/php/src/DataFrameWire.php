<?php

declare(strict_types=1);

namespace Aeon\ProcessorSdk;

/**
 * T4 WebSocket data frame with routing header.
 * Format: [4B name_len LE][pipeline_name UTF-8][2B partition LE][batch_data]
 */
final class DataFrameWire
{
    /** Build a data frame with routing header. */
    public static function build(string $pipelineName, int $partition, string $batchData): string
    {
        $nameLen = strlen($pipelineName);
        return pack('V', $nameLen) . $pipelineName . pack('v', $partition) . $batchData;
    }

    /** Parse a data frame routing header. */
    public static function parse(string $data): DataFrame
    {
        $len = strlen($data);
        if ($len < 6) {
            throw new \RuntimeException("Data frame too short: $len");
        }

        $nameLen = BatchWire::readUint32LE($data, 0);
        if (4 + $nameLen + 2 > $len) {
            throw new \RuntimeException('Data frame name extends beyond buffer');
        }

        $pipelineName = substr($data, 4, $nameLen);
        $partition = BatchWire::readUint16LE($data, 4 + $nameLen);
        $batchData = substr($data, 4 + $nameLen + 2);

        return new DataFrame($pipelineName, $partition, $batchData);
    }
}
