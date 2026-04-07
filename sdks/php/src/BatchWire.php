<?php

declare(strict_types=1);

namespace Aeon\ProcessorSdk;

/**
 * Batch wire format encoder/decoder for the AWPP protocol.
 *
 * Request:  [8B batch_id LE][4B count LE][per event: 4B len LE + codec bytes][4B CRC32 LE]
 * Response: [8B batch_id LE][4B count LE][per event: 4B output_count LE [per output: 4B len LE + codec bytes]][4B CRC32 LE][64B ED25519 sig]
 */
final class BatchWire
{
    /** Decode a batch request from binary wire format. */
    public static function decodeBatchRequest(string $data, Codec $codec): BatchRequest
    {
        $len = strlen($data);
        if ($len < 16) {
            throw new \RuntimeException("Batch request too short: $len bytes");
        }

        $batchId = self::readUint64LE($data, 0);
        $eventCount = self::readUint32LE($data, 8);

        $payloadEnd = $len - 4;
        $expectedCrc = self::readUint32LE($data, $payloadEnd);
        $actualCrc = Crc32::compute(substr($data, 0, $payloadEnd));
        if ($expectedCrc !== $actualCrc) {
            throw new \RuntimeException("CRC32 mismatch: expected $expectedCrc, got $actualCrc");
        }

        $events = [];
        $offset = 12;
        for ($i = 0; $i < $eventCount; $i++) {
            if ($offset + 4 > $payloadEnd) {
                throw new \RuntimeException("Unexpected end of batch at event $i");
            }
            $eventLen = self::readUint32LE($data, $offset);
            $offset += 4;
            if ($offset + $eventLen > $payloadEnd) {
                throw new \RuntimeException("Event $i extends beyond batch");
            }
            $events[] = $codec->decodeEvent(substr($data, $offset, $eventLen));
            $offset += $eventLen;
        }

        return new BatchRequest($batchId, $events);
    }

    /**
     * Encode a batch response to binary wire format.
     * @param list<list<Output>> $outputsPerEvent
     */
    public static function encodeBatchResponse(
        int $batchId,
        array $outputsPerEvent,
        Codec $codec,
        ?Signer $signer,
        bool $batchSigning,
    ): string {
        $payload = self::packUint64LE($batchId) . pack('V', count($outputsPerEvent));

        foreach ($outputsPerEvent as $outputs) {
            $payload .= pack('V', count($outputs));
            foreach ($outputs as $output) {
                $encoded = $codec->encodeOutput($output);
                $payload .= pack('V', strlen($encoded)) . $encoded;
            }
        }

        $crc = Crc32::compute($payload);
        $crcBuf = pack('V', $crc);

        if ($batchSigning && $signer !== null) {
            $sig = $signer->signBatch($payload . $crcBuf);
        } else {
            $sig = str_repeat("\x00", 64);
        }

        return $payload . $crcBuf . $sig;
    }

    public static function readUint32LE(string $data, int $offset): int
    {
        return unpack('V', substr($data, $offset, 4))[1];
    }

    public static function readUint64LE(string $data, int $offset): int
    {
        return unpack('P', substr($data, $offset, 8))[1];
    }

    public static function readUint16LE(string $data, int $offset): int
    {
        return unpack('v', substr($data, $offset, 2))[1];
    }

    public static function packUint64LE(int $value): string
    {
        return pack('P', $value);
    }
}
