<?php

declare(strict_types=1);

namespace Aeon\ProcessorSdk\Adapter;

use Aeon\ProcessorSdk\Processor;

/**
 * WebSocket transport adapter interface.
 *
 * Each deployment model (Swoole, RevoltPHP+ReactPHP, RevoltPHP+AMPHP,
 * Workerman, FrankenPHP/RoadRunner, Native CLI) implements this interface
 * to provide WebSocket connectivity for the AWPP protocol.
 */
interface AdapterInterface
{
    /**
     * Connect to the Aeon engine and run the processor loop.
     *
     * @param string $url WebSocket URL (e.g., ws://localhost:4471/api/v1/processors/connect)
     * @param array{
     *     name: string,
     *     version?: string,
     *     privateKeyPath?: string,
     *     privateKey?: string,
     *     pipelines: list<string>,
     *     codec?: string,
     *     maxBatchSize?: int,
     *     binding?: string,
     * } $config Runner configuration.
     * @param Processor $processor Registered processor.
     */
    public function run(string $url, array $config, Processor $processor): void;
}
