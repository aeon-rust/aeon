/**
 * Tests for Aeon Node.js processor SDK.
 *
 * Run: node --test test_aeon.js
 */

'use strict';

const { describe, it } = require('node:test');
const assert = require('node:assert/strict');
const { Buffer } = require('node:buffer');

const {
    Codec,
    Signer,
    crc32,
    decodeBatchRequest,
    encodeBatchResponse,
    buildDataFrame,
    parseDataFrame,
    processor,
    batchProcessor,
    outputWithIdentity,
    CODEC_MSGPACK,
    CODEC_JSON,
} = require('./aeon.js');

// ── CRC32 Tests ────────────────────────────────────────────────────────

describe('CRC32', () => {
    it('should compute CRC32 for empty buffer', () => {
        assert.equal(crc32(Buffer.alloc(0)), 0);
    });

    it('should compute CRC32 for known value', () => {
        // "123456789" has well-known CRC32 = 0xCBF43926
        const buf = Buffer.from('123456789', 'ascii');
        assert.equal(crc32(buf), 0xCBF43926);
    });

    it('should compute CRC32 for binary data', () => {
        const buf = Buffer.from([0x00, 0xFF, 0x55, 0xAA]);
        const result = crc32(buf);
        assert.equal(typeof result, 'number');
        assert.ok(result >= 0);
    });
});

// ── Signer Tests ───────────────────────────────────────────────────────

describe('Signer', () => {
    it('should generate a keypair', () => {
        const signer = Signer.generate();
        assert.equal(signer.publicKeyHex.length, 64);
        assert.ok(signer.publicKeyFormatted.startsWith('ed25519:'));
        assert.equal(signer.fingerprint.length, 64);
    });

    it('should create from seed', () => {
        const seed = Buffer.alloc(32, 0x42);
        const signer = Signer.fromSeed(seed);
        // Same seed should produce same key
        const signer2 = Signer.fromSeed(seed);
        assert.equal(signer.publicKeyHex, signer2.publicKeyHex);
    });

    it('should sign and verify challenge', () => {
        const signer = Signer.generate();
        const nonce = Buffer.from('deadbeef'.repeat(8), 'hex');
        const nonceHex = nonce.toString('hex');
        const sigHex = signer.signChallenge(nonceHex);
        assert.equal(sigHex.length, 128); // 64 bytes = 128 hex chars
    });

    it('should sign and verify batch data', () => {
        const signer = Signer.generate();
        const data = Buffer.from('test batch data');
        const sig = signer.signBatch(data);
        assert.equal(sig.length, 64);
        assert.ok(signer.verify(data, sig));
    });

    it('should fail verification with wrong data', () => {
        const signer = Signer.generate();
        const data = Buffer.from('original data');
        const sig = signer.signBatch(data);
        const tampered = Buffer.from('tampered data');
        assert.ok(!signer.verify(tampered, sig));
    });

    it('should produce different keys from different seeds', () => {
        const s1 = Signer.fromSeed(Buffer.alloc(32, 0x01));
        const s2 = Signer.fromSeed(Buffer.alloc(32, 0x02));
        assert.notEqual(s1.publicKeyHex, s2.publicKeyHex);
    });
});

// ── Codec Tests ────────────────────────────────────────────────────────

describe('Codec', () => {
    for (const codecName of [CODEC_MSGPACK, CODEC_JSON]) {
        describe(`${codecName}`, () => {
            let codec;

            it('should initialize', async () => {
                codec = new Codec(codecName);
                await codec._ensureLoaded();
            });

            it('should roundtrip event', () => {
                const event = {
                    id: '550e8400-e29b-41d4-a716-446655440000',
                    timestamp: 1712345678000000000,
                    source: 'test-source',
                    partition: 3,
                    metadata: [['key1', 'val1']],
                    payload: Buffer.from('hello world'),
                    sourceOffset: 42,
                };
                const encoded = codec.encodeEvent(event);
                const decoded = codec.decodeEvent(encoded);
                assert.equal(decoded.id, event.id);
                assert.equal(decoded.timestamp, event.timestamp);
                assert.equal(decoded.source, event.source);
                assert.equal(decoded.partition, event.partition);
                assert.deepEqual(decoded.metadata, event.metadata);
                assert.ok(Buffer.from(decoded.payload).equals(event.payload));
                assert.equal(decoded.sourceOffset, 42);
            });

            it('should roundtrip event without optional fields', () => {
                const event = {
                    id: '550e8400-e29b-41d4-a716-446655440000',
                    timestamp: 0,
                    source: 'src',
                    partition: 0,
                    metadata: [],
                    payload: Buffer.alloc(0),
                    sourceOffset: null,
                };
                const encoded = codec.encodeEvent(event);
                const decoded = codec.decodeEvent(encoded);
                assert.equal(decoded.id, event.id);
                assert.equal(decoded.sourceOffset, null);
            });

            it('should roundtrip output', () => {
                const output = {
                    destination: 'sink-topic',
                    payload: Buffer.from('output data'),
                    key: Buffer.from('my-key'),
                    headers: [['h1', 'v1'], ['h2', 'v2']],
                    sourceEventId: '550e8400-e29b-41d4-a716-446655440000',
                    sourcePartition: 5,
                    sourceOffset: 99,
                };
                const encoded = codec.encodeOutput(output);
                const decoded = codec.decodeOutput(encoded);
                assert.equal(decoded.destination, output.destination);
                assert.ok(Buffer.from(decoded.payload).equals(output.payload));
                assert.ok(Buffer.from(decoded.key).equals(output.key));
                assert.deepEqual(decoded.headers, output.headers);
                assert.equal(decoded.sourceEventId, output.sourceEventId);
                assert.equal(decoded.sourcePartition, 5);
                assert.equal(decoded.sourceOffset, 99);
            });

            it('should roundtrip output without key', () => {
                const output = {
                    destination: 'out',
                    payload: Buffer.from('data'),
                    key: null,
                    headers: [],
                    sourceEventId: null,
                    sourcePartition: null,
                    sourceOffset: null,
                };
                const encoded = codec.encodeOutput(output);
                const decoded = codec.decodeOutput(encoded);
                assert.equal(decoded.destination, 'out');
                assert.equal(decoded.key, null);
            });
        });
    }
});

// ── Data Frame Tests ───────────────────────────────────────────────────

describe('Data Frame', () => {
    it('should roundtrip data frame', () => {
        const pipelineName = 'my-pipeline';
        const partition = 7;
        const batchData = Buffer.from('batch-content');

        const frame = buildDataFrame(pipelineName, partition, batchData);
        const parsed = parseDataFrame(frame);

        assert.equal(parsed.pipelineName, pipelineName);
        assert.equal(parsed.partition, partition);
        assert.ok(parsed.batchData.equals(batchData));
    });

    it('should handle empty pipeline name', () => {
        const frame = buildDataFrame('', 0, Buffer.from('data'));
        const parsed = parseDataFrame(frame);
        assert.equal(parsed.pipelineName, '');
        assert.equal(parsed.partition, 0);
    });

    it('should handle unicode pipeline name', () => {
        const name = 'pipeline-日本語';
        const frame = buildDataFrame(name, 100, Buffer.from('x'));
        const parsed = parseDataFrame(frame);
        assert.equal(parsed.pipelineName, name);
    });

    it('should reject too-short frame', () => {
        assert.throws(() => parseDataFrame(Buffer.alloc(3)), /too short/);
    });
});

// ── Batch Wire Format Tests ────────────────────────────────────────────

describe('Batch Wire Format', () => {
    it('should encode and verify batch response', async () => {
        const codec = new Codec(CODEC_MSGPACK);
        await codec._ensureLoaded();
        const signer = Signer.generate();

        const outputsPerEvent = [
            [{ destination: 'out', payload: Buffer.from('hello'), key: null, headers: [] }],
            [], // event with no outputs
        ];

        const response = encodeBatchResponse(
            42n,
            outputsPerEvent,
            codec,
            signer,
            true,
        );

        // Verify structure: header(12) + outputs + CRC(4) + sig(64)
        assert.ok(response.length > 12 + 4 + 64);

        // Verify batch_id
        assert.equal(response.readBigUInt64LE(0), 42n);
        // Verify event_count
        assert.equal(response.readUInt32LE(8), 2);

        // Verify CRC32
        const payloadEnd = response.length - 64 - 4;
        // CRC is at payloadEnd
        const crcExpected = response.readUInt32LE(payloadEnd);
        const crcActual = crc32(response.subarray(0, payloadEnd));
        assert.equal(crcExpected, crcActual);

        // Verify signature
        const sigStart = response.length - 64;
        const signedData = response.subarray(0, sigStart);
        const sig = response.subarray(sigStart);
        assert.ok(signer.verify(signedData, sig));
    });

    it('should encode unsigned batch response', async () => {
        const codec = new Codec(CODEC_MSGPACK);
        await codec._ensureLoaded();

        const response = encodeBatchResponse(
            1n,
            [[{ destination: 'out', payload: Buffer.from('x'), key: null, headers: [] }]],
            codec,
            null,
            false,
        );

        // Last 64 bytes should be zeros (no signing)
        const sig = response.subarray(response.length - 64);
        assert.ok(sig.equals(Buffer.alloc(64, 0)));
    });

    it('should roundtrip batch request → decode', async () => {
        const codec = new Codec(CODEC_MSGPACK);
        await codec._ensureLoaded();

        // Manually build a batch request
        const batchId = 99n;
        const events = [
            {
                id: 'abc-123',
                timestamp: 1000,
                source: 'src',
                partition: 0,
                metadata: [],
                payload: Buffer.from('event-data'),
                sourceOffset: null,
            },
        ];

        // Encode batch request manually
        const header = Buffer.alloc(12);
        header.writeBigUInt64LE(batchId, 0);
        header.writeUInt32LE(events.length, 8);

        const eventParts = [];
        for (const event of events) {
            const encoded = codec.encodeEvent(event);
            const lenBuf = Buffer.alloc(4);
            lenBuf.writeUInt32LE(encoded.length, 0);
            eventParts.push(lenBuf);
            eventParts.push(encoded);
        }

        const payload = Buffer.concat([header, ...eventParts]);
        const crcVal = crc32(payload);
        const crcBuf = Buffer.alloc(4);
        crcBuf.writeUInt32LE(crcVal, 0);
        const request = Buffer.concat([payload, crcBuf]);

        // Decode
        const decoded = decodeBatchRequest(request, codec);
        assert.equal(decoded.batchId, batchId);
        assert.equal(decoded.events.length, 1);
        assert.equal(decoded.events[0].id, 'abc-123');
        assert.ok(Buffer.from(decoded.events[0].payload).equals(Buffer.from('event-data')));
    });

    it('should reject batch with CRC mismatch', async () => {
        const codec = new Codec(CODEC_MSGPACK);
        await codec._ensureLoaded();

        const header = Buffer.alloc(12);
        header.writeBigUInt64LE(1n, 0);
        header.writeUInt32LE(0, 8);
        const badCrc = Buffer.alloc(4);
        badCrc.writeUInt32LE(0xDEADBEEF, 0);
        const request = Buffer.concat([header, badCrc]);

        assert.throws(() => decodeBatchRequest(request, codec), /CRC32 mismatch/);
    });

    it('should handle multiple events in batch', async () => {
        const codec = new Codec(CODEC_JSON);
        await codec._ensureLoaded();

        const batchId = 7n;
        const events = [];
        for (let i = 0; i < 5; i++) {
            events.push({
                id: `event-${i}`,
                timestamp: i * 1000,
                source: 'src',
                partition: i % 4,
                metadata: [['idx', `${i}`]],
                payload: Buffer.from(`payload-${i}`),
                sourceOffset: i,
            });
        }

        // Build request
        const header = Buffer.alloc(12);
        header.writeBigUInt64LE(batchId, 0);
        header.writeUInt32LE(events.length, 8);
        const eventParts = [];
        for (const event of events) {
            const encoded = codec.encodeEvent(event);
            const lenBuf = Buffer.alloc(4);
            lenBuf.writeUInt32LE(encoded.length, 0);
            eventParts.push(lenBuf);
            eventParts.push(encoded);
        }
        const payload = Buffer.concat([header, ...eventParts]);
        const crcBuf = Buffer.alloc(4);
        crcBuf.writeUInt32LE(crc32(payload), 0);
        const request = Buffer.concat([payload, crcBuf]);

        const decoded = decodeBatchRequest(request, codec);
        assert.equal(decoded.batchId, batchId);
        assert.equal(decoded.events.length, 5);
        for (let i = 0; i < 5; i++) {
            assert.equal(decoded.events[i].id, `event-${i}`);
            assert.equal(decoded.events[i].partition, i % 4);
            assert.equal(decoded.events[i].sourceOffset, i);
        }
    });
});

// ── Processor Registration Tests ───────────────────────────────────────

describe('Processor Registration', () => {
    it('should create per-event processor', () => {
        const p = processor((event) => [{
            destination: 'out',
            payload: event.payload,
        }]);
        assert.ok(p.processFn);
        assert.equal(p.batchProcessFn, null);
    });

    it('should create batch processor', () => {
        const p = batchProcessor((events) =>
            events.map((e) => [{ destination: 'out', payload: e.payload }])
        );
        assert.equal(p.processFn, null);
        assert.ok(p.batchProcessFn);
    });

    it('should propagate event identity', () => {
        const event = {
            id: 'evt-1',
            partition: 3,
            sourceOffset: 42,
        };
        const output = { destination: 'out', payload: Buffer.from('x') };
        const result = outputWithIdentity(output, event);
        assert.equal(result.sourceEventId, 'evt-1');
        assert.equal(result.sourcePartition, 3);
        assert.equal(result.sourceOffset, 42);
    });
});

// ── Cross-Codec Compatibility ──────────────────────────────────────────

describe('Cross-Codec Compatibility', () => {
    it('should produce compatible batch wire format across codecs', async () => {
        for (const codecName of [CODEC_MSGPACK, CODEC_JSON]) {
            const codec = new Codec(codecName);
            await codec._ensureLoaded();
            const signer = Signer.generate();

            const outputs = [[
                {
                    destination: 'topic-out',
                    payload: Buffer.from('binary-data'),
                    key: Buffer.from('key-1'),
                    headers: [['content-type', 'application/octet-stream']],
                    sourceEventId: null,
                    sourcePartition: null,
                    sourceOffset: null,
                },
            ]];

            const response = encodeBatchResponse(1n, outputs, codec, signer, true);

            // Verify basic structure
            assert.equal(response.readBigUInt64LE(0), 1n);
            assert.equal(response.readUInt32LE(8), 1); // 1 event

            // Verify CRC
            const crcOffset = response.length - 64 - 4;
            const expectedCrc = response.readUInt32LE(crcOffset);
            const actualCrc = crc32(response.subarray(0, crcOffset));
            assert.equal(expectedCrc, actualCrc, `CRC mismatch for ${codecName}`);

            // Verify signature
            const signedData = response.subarray(0, response.length - 64);
            const sig = response.subarray(response.length - 64);
            assert.ok(signer.verify(signedData, sig), `Signature invalid for ${codecName}`);
        }
    });
});
