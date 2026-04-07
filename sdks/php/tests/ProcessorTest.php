<?php

declare(strict_types=1);

namespace Aeon\ProcessorSdk\Tests;

use Aeon\ProcessorSdk\{Event, Output, Processor};
use PHPUnit\Framework\TestCase;

class ProcessorTest extends TestCase
{
    public function testPerEventProcessor(): void
    {
        $p = Processor::perEvent(fn(Event $e) => [
            new Output(destination: 'out', payload: $e->payload),
        ]);
        $this->assertNotNull($p->getProcessFn());
        $this->assertNull($p->getBatchProcessFn());
    }

    public function testBatchProcessor(): void
    {
        $p = Processor::batch(fn(array $events) => array_map(
            fn(Event $e) => [new Output(destination: 'out', payload: $e->payload)],
            $events,
        ));
        $this->assertNull($p->getProcessFn());
        $this->assertNotNull($p->getBatchProcessFn());
    }

    public function testOutputWithEventIdentity(): void
    {
        $event = new Event(id: 'evt-1', timestamp: 0, source: 's', partition: 3, metadata: [], payload: '', sourceOffset: 42);
        $output = new Output(destination: 'out', payload: 'x');
        $result = $output->withEventIdentity($event);
        $this->assertSame('evt-1', $result->sourceEventId);
        $this->assertSame(3, $result->sourcePartition);
        $this->assertSame(42, $result->sourceOffset);
    }

    public function testPerEventProcessorReturnsCorrectOutputs(): void
    {
        $p = Processor::perEvent(fn(Event $e) => [
            new Output(destination: 'out', payload: $e->payload),
        ]);
        $events = [new Event(id: 'test', timestamp: 0, source: 's', partition: 0, metadata: [], payload: 'hello')];
        $results = $p->processEvents($events);
        $this->assertCount(1, $results);
        $this->assertCount(1, $results[0]);
        $this->assertSame('out', $results[0][0]->destination);
        $this->assertSame('hello', $results[0][0]->payload);
    }

    public function testOutputWithKey(): void
    {
        $output = new Output(destination: 'out', payload: 'data');
        $keyed = $output->withKey('partition-key');
        $this->assertSame('partition-key', $keyed->key);
        $this->assertSame('out', $keyed->destination);
    }

    public function testOutputWithHeader(): void
    {
        $output = new Output(destination: 'out', payload: 'data');
        $h = $output->withHeader('content-type', 'application/json');
        $this->assertCount(1, $h->headers);
        $this->assertSame('content-type', $h->headers[0][0]);
    }

    public function testOutputImmutable(): void
    {
        $o1 = new Output(destination: 'a', payload: 'x');
        $o2 = $o1->withKey('k');
        $this->assertNull($o1->key);
        $this->assertSame('k', $o2->key);
    }

    public function testEventPayloadString(): void
    {
        $event = new Event(id: 'x', timestamp: 0, source: 's', partition: 0, metadata: [], payload: 'hello');
        $this->assertSame('hello', $event->payloadString());
    }
}
