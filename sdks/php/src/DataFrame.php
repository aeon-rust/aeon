<?php

declare(strict_types=1);

namespace Aeon\ProcessorSdk;

/** Parsed data frame. */
final class DataFrame
{
    public function __construct(
        public readonly string $pipelineName,
        public readonly int $partition,
        public readonly string $batchData,
    ) {}
}
