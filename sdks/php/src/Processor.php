<?php

declare(strict_types=1);

namespace Aeon\ProcessorSdk;

/**
 * Processor registration — per-event or batch.
 */
final class Processor
{
    /** @var \Closure(Event): list<Output>|null */
    private ?\Closure $processFn;
    /** @var \Closure(list<Event>): list<list<Output>>|null */
    private ?\Closure $batchProcessFn;

    private function __construct(?\Closure $processFn, ?\Closure $batchProcessFn)
    {
        $this->processFn = $processFn;
        $this->batchProcessFn = $batchProcessFn;
    }

    /**
     * Register a per-event processor function.
     *
     * @param callable(Event): list<Output> $fn
     */
    public static function perEvent(callable $fn): self
    {
        return new self(\Closure::fromCallable($fn), null);
    }

    /**
     * Register a batch processor function.
     *
     * @param callable(list<Event>): list<list<Output>> $fn
     */
    public static function batch(callable $fn): self
    {
        return new self(null, \Closure::fromCallable($fn));
    }

    /** @return \Closure(Event): list<Output>|null */
    public function getProcessFn(): ?\Closure
    {
        return $this->processFn;
    }

    /** @return \Closure(list<Event>): list<list<Output>>|null */
    public function getBatchProcessFn(): ?\Closure
    {
        return $this->batchProcessFn;
    }

    /**
     * Process events using the registered function.
     *
     * @param list<Event> $events
     * @return list<list<Output>>
     */
    public function processEvents(array $events): array
    {
        if ($this->batchProcessFn !== null) {
            return ($this->batchProcessFn)($events);
        }
        if ($this->processFn !== null) {
            $fn = $this->processFn;
            return array_map(fn(Event $e) => $fn($e), $events);
        }
        return array_map(fn() => [], $events);
    }
}
