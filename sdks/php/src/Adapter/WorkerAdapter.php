<?php

declare(strict_types=1);

namespace Aeon\ProcessorSdk\Adapter;

use Aeon\ProcessorSdk\{BatchWire, Codec, DataFrameWire, Processor, Signer};

/**
 * FrankenPHP / RoadRunner worker mode adapter.
 *
 * Operates as a persistent PHP worker that communicates with the Aeon engine
 * via WebSocket. The worker stays alive across requests, avoiding PHP startup
 * overhead.
 *
 * For FrankenPHP: Uses the worker mode API (frankenphp_handle_request).
 * For RoadRunner: Uses the Goridge protocol (spiral/roadrunner-worker).
 *
 * In practice, both FrankenPHP and RoadRunner keep the PHP process alive,
 * so the WebSocket connection persists. This adapter uses the NativeCliAdapter
 * internally since the worker process has full socket access.
 */
final class WorkerAdapter implements AdapterInterface
{
    public function run(string $url, array $config, Processor $processor): void
    {
        // Detect runtime
        $isFrankenPHP = function_exists('frankenphp_handle_request');
        $isRoadRunner = class_exists(\Spiral\RoadRunner\Worker::class);

        if ($isFrankenPHP) {
            $this->runFrankenPHP($url, $config, $processor);
        } elseif ($isRoadRunner) {
            $this->runRoadRunner($url, $config, $processor);
        } else {
            // Fallback: run as a long-lived CLI process (same as native)
            $native = new NativeCliAdapter();
            $native->run($url, $config, $processor);
        }
    }

    private function runFrankenPHP(string $url, array $config, Processor $processor): void
    {
        // FrankenPHP worker mode: the process stays alive, we just run the
        // WebSocket client loop directly. frankenphp_handle_request is for
        // HTTP mode; in worker mode we bypass it for long-running tasks.
        $native = new NativeCliAdapter();
        $native->run($url, $config, $processor);
    }

    private function runRoadRunner(string $url, array $config, Processor $processor): void
    {
        // RoadRunner worker: similar to FrankenPHP, the process is long-lived.
        // We use the native socket approach since we have full process control.
        $native = new NativeCliAdapter();
        $native->run($url, $config, $processor);
    }
}
