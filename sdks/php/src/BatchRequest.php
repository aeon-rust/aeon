<?php

declare(strict_types=1);

namespace Aeon\ProcessorSdk;

/** Decoded batch request. */
final class BatchRequest
{
    /** @param list<Event> $events */
    public function __construct(
        public readonly int $batchId,
        public readonly array $events,
    ) {}
}
