<?php

declare(strict_types=1);

namespace Aeon\ProcessorSdk;

/**
 * CRC32 (IEEE) implementation matching the AWPP batch wire protocol.
 * Uses PHP's built-in crc32() which uses the IEEE polynomial.
 */
final class Crc32
{
    /**
     * Compute CRC32 (IEEE) of a binary string.
     * Returns unsigned 32-bit integer.
     */
    public static function compute(string $data): int
    {
        return crc32($data) & 0xFFFFFFFF;
    }
}
