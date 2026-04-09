<?php

declare(strict_types=1);

namespace Aeon\ProcessorSdk;

/** Outbound record to be written to a sink. */
final class Output
{
    /** @param list<array{0: string, 1: string}> $headers */
    public function __construct(
        public readonly string $destination,
        public readonly string $payload,
        public readonly ?string $key = null,
        public readonly array $headers = [],
        public readonly ?string $sourceEventId = null,
        public readonly ?int $sourcePartition = null,
        public readonly ?int $sourceOffset = null,
    ) {}

    public function withEventIdentity(Event $event): self
    {
        return new self(
            destination: $this->destination,
            payload: $this->payload,
            key: $this->key,
            headers: $this->headers,
            sourceEventId: $event->id,
            sourcePartition: $event->partition,
            sourceOffset: $event->sourceOffset,
        );
    }

    public function withKey(string $key): self
    {
        return new self(
            destination: $this->destination,
            payload: $this->payload,
            key: $key,
            headers: $this->headers,
            sourceEventId: $this->sourceEventId,
            sourcePartition: $this->sourcePartition,
            sourceOffset: $this->sourceOffset,
        );
    }

    public function withHeader(string $key, string $value): self
    {
        $headers = $this->headers;
        $headers[] = [$key, $value];
        return new self(
            destination: $this->destination,
            payload: $this->payload,
            key: $this->key,
            headers: $headers,
            sourceEventId: $this->sourceEventId,
            sourcePartition: $this->sourcePartition,
            sourceOffset: $this->sourceOffset,
        );
    }
}
