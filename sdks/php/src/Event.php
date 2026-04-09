<?php

declare(strict_types=1);

namespace Aeon\ProcessorSdk;

/** Inbound event from the Aeon engine. */
final class Event
{
    /** @param list<array{0: string, 1: string}> $metadata */
    public function __construct(
        public readonly string $id,
        public readonly int $timestamp,
        public readonly string $source,
        public readonly int $partition,
        public readonly array $metadata,
        public readonly string $payload,
        public readonly ?int $sourceOffset = null,
    ) {}

    public function payloadString(): string
    {
        return $this->payload;
    }
}
