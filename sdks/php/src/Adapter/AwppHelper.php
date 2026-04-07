<?php

declare(strict_types=1);

namespace Aeon\ProcessorSdk\Adapter;

use Aeon\ProcessorSdk\{BatchWire, Codec, DataFrameWire, Processor, Signer};

/**
 * Shared AWPP handshake and batch processing logic used by all adapters.
 *
 * This class contains the protocol-level logic that is adapter-agnostic:
 * challenge-response authentication, control message handling, and batch
 * frame processing.
 */
final class AwppHelper
{
    /**
     * Create a Signer from config.
     */
    public static function createSigner(array $config): Signer
    {
        if (isset($config['privateKeyPath'])) {
            return Signer::fromFile($config['privateKeyPath']);
        }
        if (isset($config['privateKey'])) {
            return Signer::fromSeed($config['privateKey']);
        }
        return Signer::generate();
    }

    /**
     * Handle an AWPP control message (text frame).
     *
     * @return string|null JSON response to send, or null if no response needed.
     */
    public static function handleControlMessage(
        string $data,
        Signer $signer,
        array $config,
        Codec $codec,
        bool &$handshakeComplete,
        bool &$batchSigningEnabled,
        int &$heartbeatIntervalMs,
    ): ?string {
        $msg = json_decode($data, true);
        if (!is_array($msg) || !isset($msg['type'])) {
            return null;
        }

        switch ($msg['type']) {
            case 'challenge':
                $nonce = $msg['nonce'] ?? '';
                $reg = [
                    'type' => 'register',
                    'protocol' => 'awpp/1',
                    'transport' => 'websocket',
                    'name' => $config['name'] ?? 'php-processor',
                    'version' => $config['version'] ?? '1.0.0',
                    'public_key' => $signer->publicKeyFormatted(),
                    'challenge_signature' => $signer->signChallenge($nonce),
                    'oauth_token' => null,
                    'capabilities' => ['batch'],
                    'max_batch_size' => $config['maxBatchSize'] ?? 1000,
                    'transport_codec' => $config['codec'] ?? $codec->name,
                    'requested_pipelines' => $config['pipelines'] ?? [],
                    'binding' => $config['binding'] ?? 'dedicated',
                ];
                return json_encode($reg, JSON_THROW_ON_ERROR);

            case 'accepted':
                $handshakeComplete = true;
                $heartbeatIntervalMs = $msg['heartbeat_interval_ms'] ?? 10000;
                $batchSigningEnabled = $msg['batch_signing'] ?? false;
                return null;

            case 'rejected':
                $code = $msg['code'] ?? 'unknown';
                $message = $msg['message'] ?? '';
                throw new \RuntimeException("AWPP rejected: [$code] $message");

            case 'drain':
                return null; // Signal caller to close

            case 'heartbeat':
                return null; // Server heartbeat — acknowledged by our timer

            default:
                return null;
        }
    }

    /**
     * Process a binary batch frame and return the response frame.
     */
    public static function processBatchFrame(
        string $data,
        Codec $codec,
        Signer $signer,
        bool $batchSigning,
        Processor $processor,
    ): string {
        $frame = DataFrameWire::parse($data);
        $batch = BatchWire::decodeBatchRequest($frame->batchData, $codec);

        $outputsPerEvent = $processor->processEvents($batch->events);

        $responseBatch = BatchWire::encodeBatchResponse(
            $batch->batchId, $outputsPerEvent, $codec, $signer, $batchSigning,
        );

        return DataFrameWire::build($frame->pipelineName, $frame->partition, $responseBatch);
    }
}
