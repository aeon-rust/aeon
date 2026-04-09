/**
 * Aeon Processor SDK for Node.js — T4 WebSocket client.
 *
 * Connects to the Aeon engine via WebSocket at /api/v1/processors/connect,
 * runs the AWPP handshake (ED25519 challenge-response authentication),
 * receives batch requests, calls the user's processor function, and sends
 * batch responses.
 *
 * Dependencies:
 *     npm install ws msgpackr @noble/ed25519
 *
 * Usage:
 *     const { processor, run } = require('@aeon/processor-sdk');
 *
 *     const myProcessor = processor((event) => {
 *         return [{ destination: 'out', payload: event.payload }];
 *     });
 *
 *     run('ws://localhost:4471/api/v1/processors/connect', {
 *         name: 'my-processor',
 *         version: '1.0.0',
 *         privateKeyPath: 'processor.key',
 *         pipelines: ['my-pipeline'],
 *     });
 *
 * @module @aeon/processor-sdk
 */

'use strict';

const crypto = require('node:crypto');
const fs = require('node:fs');
const { Buffer } = require('node:buffer');

// ── Codec ──────────────────────────────────────────────────────────────

const CODEC_MSGPACK = 'msgpack';
const CODEC_JSON = 'json';

class Codec {
    constructor(name = CODEC_MSGPACK) {
        this.name = name;
        this._pack = null;
        this._unpack = null;
    }

    async _ensureLoaded() {
        if (this._pack) return;
        if (this.name === CODEC_MSGPACK) {
            const msgpackr = require('msgpackr');
            this._pack = (v) => msgpackr.pack(v);
            this._unpack = (buf) => msgpackr.unpack(buf);
        } else {
            this._pack = (v) => Buffer.from(JSON.stringify(v));
            this._unpack = (buf) => JSON.parse(buf.toString('utf8'));
        }
    }

    encodeEvent(event) {
        const obj = {
            id: event.id,
            timestamp: event.timestamp,
            source: event.source,
            partition: event.partition,
            metadata: event.metadata,
            payload: this.name === CODEC_JSON
                ? Buffer.from(event.payload).toString('base64')
                : event.payload,
        };
        if (event.sourceOffset != null) obj.source_offset = event.sourceOffset;
        return this._pack(obj);
    }

    decodeEvent(buf) {
        const obj = this._unpack(buf instanceof Buffer ? buf : Buffer.from(buf));
        return {
            id: obj.id,
            timestamp: obj.timestamp,
            source: obj.source,
            partition: obj.partition,
            metadata: obj.metadata || [],
            payload: this.name === CODEC_JSON
                ? Buffer.from(obj.payload, 'base64')
                : (obj.payload instanceof Buffer ? obj.payload : Buffer.from(obj.payload)),
            sourceOffset: obj.source_offset ?? null,
        };
    }

    encodeOutput(output) {
        const obj = {
            destination: output.destination,
            payload: this.name === CODEC_JSON
                ? Buffer.from(output.payload).toString('base64')
                : output.payload,
            headers: output.headers || [],
        };
        if (output.key != null) {
            obj.key = this.name === CODEC_JSON
                ? Buffer.from(output.key).toString('base64')
                : output.key;
        }
        if (output.sourceEventId != null) obj.source_event_id = output.sourceEventId;
        if (output.sourcePartition != null) obj.source_partition = output.sourcePartition;
        if (output.sourceOffset != null) obj.source_offset = output.sourceOffset;
        return this._pack(obj);
    }

    decodeOutput(buf) {
        const obj = this._unpack(buf instanceof Buffer ? buf : Buffer.from(buf));
        return {
            destination: obj.destination,
            payload: this.name === CODEC_JSON
                ? Buffer.from(obj.payload, 'base64')
                : (obj.payload instanceof Buffer ? obj.payload : Buffer.from(obj.payload)),
            key: obj.key != null
                ? (this.name === CODEC_JSON ? Buffer.from(obj.key, 'base64') : (obj.key instanceof Buffer ? obj.key : Buffer.from(obj.key)))
                : null,
            headers: obj.headers || [],
            sourceEventId: obj.source_event_id ?? null,
            sourcePartition: obj.source_partition ?? null,
            sourceOffset: obj.source_offset ?? null,
        };
    }
}

// ── ED25519 Auth (using Node.js built-in crypto) ───────────────────────

class Signer {
    /**
     * @param {Buffer} seed - 32-byte ED25519 seed
     */
    constructor(seed) {
        this._seed = seed;
        // Node.js crypto uses PKCS8 for ed25519 keys.
        // Create keypair from seed.
        const keyObj = crypto.createPrivateKey({
            key: Buffer.concat([
                // ED25519 PKCS8 DER prefix (16 bytes) + 32-byte seed
                Buffer.from('302e020100300506032b657004220420', 'hex'),
                seed,
            ]),
            format: 'der',
            type: 'pkcs8',
        });
        this._privateKey = keyObj;
        this._publicKey = crypto.createPublicKey(keyObj);
        // Extract raw 32-byte public key
        const spkiBuf = this._publicKey.export({ type: 'spki', format: 'der' });
        // SPKI for ED25519 is 44 bytes: 12 byte header + 32 byte key
        this._publicKeyRaw = spkiBuf.subarray(spkiBuf.length - 32);
    }

    static fromFile(path) {
        const seed = fs.readFileSync(path);
        if (seed.length !== 32) {
            throw new Error(`ED25519 seed must be 32 bytes, got ${seed.length}`);
        }
        return new Signer(seed);
    }

    static generate() {
        return new Signer(crypto.randomBytes(32));
    }

    static fromSeed(seed) {
        return new Signer(Buffer.from(seed));
    }

    /** Hex-encoded 32-byte public key. */
    get publicKeyHex() {
        return this._publicKeyRaw.toString('hex');
    }

    /** "ed25519:<base64>" format for AWPP registration. */
    get publicKeyFormatted() {
        return `ed25519:${this._publicKeyRaw.toString('base64')}`;
    }

    /** SHA-256 fingerprint of the public key. */
    get fingerprint() {
        return crypto.createHash('sha256').update(this._publicKeyRaw).digest('hex');
    }

    /**
     * Sign a hex-encoded nonce for AWPP challenge-response.
     * @param {string} nonceHex - Hex-encoded nonce bytes
     * @returns {string} Hex-encoded 64-byte ED25519 signature
     */
    signChallenge(nonceHex) {
        const nonce = Buffer.from(nonceHex, 'hex');
        const sig = crypto.sign(null, nonce, this._privateKey);
        return sig.toString('hex');
    }

    /**
     * Sign batch response data.
     * @param {Buffer} data - Batch wire data to sign
     * @returns {Buffer} 64-byte ED25519 signature
     */
    signBatch(data) {
        return crypto.sign(null, data, this._privateKey);
    }

    /**
     * Verify a signature.
     * @param {Buffer} data - Signed data
     * @param {Buffer} signature - 64-byte signature
     * @returns {boolean}
     */
    verify(data, signature) {
        return crypto.verify(null, data, this._publicKey, signature);
    }
}

// ── Batch Wire Format ──────────────────────────────────────────────────

/**
 * CRC32 (IEEE) lookup table and function.
 */
const CRC32_TABLE = new Uint32Array(256);
for (let i = 0; i < 256; i++) {
    let c = i;
    for (let j = 0; j < 8; j++) {
        c = (c & 1) ? (0xEDB88320 ^ (c >>> 1)) : (c >>> 1);
    }
    CRC32_TABLE[i] = c;
}

function crc32(buf) {
    let crc = 0xFFFFFFFF;
    for (let i = 0; i < buf.length; i++) {
        crc = CRC32_TABLE[(crc ^ buf[i]) & 0xFF] ^ (crc >>> 8);
    }
    return (crc ^ 0xFFFFFFFF) >>> 0;
}

/**
 * Decode a batch request from binary wire format.
 *
 * Format:
 *   [8B batch_id LE][4B event_count LE]
 *   [per event: [4B event_len LE][event_bytes]]
 *   [4B CRC32 LE]
 *
 * @param {Buffer} data - Raw batch wire data
 * @param {Codec} codec - Codec for deserializing events
 * @returns {{ batchId: bigint, events: Array }}
 */
function decodeBatchRequest(data, codec) {
    if (data.length < 16) {
        throw new Error(`Batch request too short: ${data.length} bytes`);
    }

    const batchId = data.readBigUInt64LE(0);
    const eventCount = data.readUInt32LE(8);

    // Verify CRC32: covers everything except the last 4 bytes
    const payloadEnd = data.length - 4;
    const expectedCrc = data.readUInt32LE(payloadEnd);
    const actualCrc = crc32(data.subarray(0, payloadEnd));
    if (expectedCrc !== actualCrc) {
        throw new Error(`CRC32 mismatch: expected ${expectedCrc}, got ${actualCrc}`);
    }

    const events = [];
    let offset = 12;
    for (let i = 0; i < eventCount; i++) {
        if (offset + 4 > payloadEnd) {
            throw new Error(`Unexpected end of batch at event ${i}`);
        }
        const eventLen = data.readUInt32LE(offset);
        offset += 4;
        if (offset + eventLen > payloadEnd) {
            throw new Error(`Event ${i} extends beyond batch`);
        }
        const eventBuf = data.subarray(offset, offset + eventLen);
        events.push(codec.decodeEvent(eventBuf));
        offset += eventLen;
    }

    return { batchId, events };
}

/**
 * Encode a batch response to binary wire format.
 *
 * Format:
 *   [8B batch_id LE][4B event_count LE]
 *   [per event:
 *     [4B output_count LE]
 *     [per output: [4B output_len LE][output_bytes]]
 *   ]
 *   [4B CRC32 LE]
 *   [64B ED25519 signature]
 *
 * @param {bigint} batchId
 * @param {Array<Array>} outputsPerEvent - Array of output arrays, one per input event
 * @param {Codec} codec
 * @param {Signer|null} signer
 * @param {boolean} batchSigning
 * @returns {Buffer}
 */
function encodeBatchResponse(batchId, outputsPerEvent, codec, signer, batchSigning) {
    const parts = [];

    // Header: batch_id + event_count
    const header = Buffer.alloc(12);
    header.writeBigUInt64LE(batchId, 0);
    header.writeUInt32LE(outputsPerEvent.length, 8);
    parts.push(header);

    // Per-event outputs
    for (const outputs of outputsPerEvent) {
        const countBuf = Buffer.alloc(4);
        countBuf.writeUInt32LE(outputs.length, 0);
        parts.push(countBuf);
        for (const output of outputs) {
            const encoded = codec.encodeOutput(output);
            const lenBuf = Buffer.alloc(4);
            lenBuf.writeUInt32LE(encoded.length, 0);
            parts.push(lenBuf);
            parts.push(encoded);
        }
    }

    // CRC32
    const payload = Buffer.concat(parts);
    const crcVal = crc32(payload);
    const crcBuf = Buffer.alloc(4);
    crcBuf.writeUInt32LE(crcVal, 0);

    // Signature
    let sigBuf;
    if (batchSigning && signer) {
        const toSign = Buffer.concat([payload, crcBuf]);
        sigBuf = signer.signBatch(toSign);
    } else {
        sigBuf = Buffer.alloc(64, 0);
    }

    return Buffer.concat([payload, crcBuf, sigBuf]);
}

// ── Data Frame (T4 WebSocket routing header) ───────────────────────────

/**
 * Build a WebSocket data frame with routing header.
 *
 * Format: [4B name_len LE][pipeline_name UTF-8][2B partition LE][batch_data]
 */
function buildDataFrame(pipelineName, partition, batchData) {
    const nameBytes = Buffer.from(pipelineName, 'utf8');
    const header = Buffer.alloc(4 + nameBytes.length + 2);
    header.writeUInt32LE(nameBytes.length, 0);
    nameBytes.copy(header, 4);
    header.writeUInt16LE(partition, 4 + nameBytes.length);
    return Buffer.concat([header, batchData]);
}

/**
 * Parse a WebSocket data frame routing header.
 *
 * @returns {{ pipelineName: string, partition: number, batchData: Buffer }}
 */
function parseDataFrame(data) {
    if (data.length < 6) {
        throw new Error(`Data frame too short: ${data.length}`);
    }
    const nameLen = data.readUInt32LE(0);
    if (4 + nameLen + 2 > data.length) {
        throw new Error(`Data frame name extends beyond buffer`);
    }
    const pipelineName = data.subarray(4, 4 + nameLen).toString('utf8');
    const partition = data.readUInt16LE(4 + nameLen);
    const batchData = data.subarray(4 + nameLen + 2);
    return { pipelineName, partition, batchData };
}

// ── Processor Decorators ───────────────────────────────────────────────

/**
 * Register a per-event processor function.
 * @param {function} fn - (event) => Array<Output>
 * @returns {{ processFn: function, batchProcessFn: null }}
 */
function processor(fn) {
    return { processFn: fn, batchProcessFn: null };
}

/**
 * Register a batch processor function.
 * @param {function} fn - (events: Array<Event>) => Array<Array<Output>>
 * @returns {{ processFn: null, batchProcessFn: function }}
 */
function batchProcessor(fn) {
    return { processFn: null, batchProcessFn: fn };
}

// ── Output Builder ─────────────────────────────────────────────────────

/**
 * Create an output with event identity propagation.
 */
function outputWithIdentity(output, event) {
    return {
        ...output,
        sourceEventId: event.id,
        sourcePartition: event.partition,
        sourceOffset: event.sourceOffset,
    };
}

// ── WebSocket Runner (T4) ──────────────────────────────────────────────

/**
 * Run a processor connected to the Aeon engine via WebSocket.
 *
 * @param {string} url - WebSocket URL (e.g., ws://localhost:4471/api/v1/processors/connect)
 * @param {object} config
 * @param {string} config.name - Processor name
 * @param {string} config.version - Processor version
 * @param {string} [config.privateKeyPath] - Path to 32-byte ED25519 seed file
 * @param {Buffer} [config.privateKey] - 32-byte ED25519 seed
 * @param {string[]} config.pipelines - Requested pipeline names
 * @param {string} [config.codec='msgpack'] - Transport codec
 * @param {object} config.processor - Result of processor() or batchProcessor()
 * @param {AbortSignal} [config.signal] - AbortSignal for graceful shutdown
 */
async function run(url, config) {
    const WebSocket = require('ws');

    const signer = config.privateKeyPath
        ? Signer.fromFile(config.privateKeyPath)
        : config.privateKey
            ? Signer.fromSeed(config.privateKey)
            : Signer.generate();

    const codecName = config.codec || CODEC_MSGPACK;
    const codec = new Codec(codecName);
    await codec._ensureLoaded();

    const { processFn, batchProcessFn } = config.processor;

    return new Promise((resolve, reject) => {
        const ws = new WebSocket(url);
        ws.binaryType = 'nodebuffer';

        let sessionId = null;
        let heartbeatInterval = 10000;
        let batchSigningEnabled = false;
        let heartbeatTimer = null;
        let handshakeComplete = false;

        function sendHeartbeat() {
            if (ws.readyState !== WebSocket.OPEN) return;
            const msg = JSON.stringify({
                type: 'heartbeat',
                timestamp_ms: Date.now(),
            });
            ws.send(msg);
        }

        ws.on('error', (err) => {
            if (heartbeatTimer) clearInterval(heartbeatTimer);
            reject(err);
        });

        ws.on('close', () => {
            if (heartbeatTimer) clearInterval(heartbeatTimer);
            resolve();
        });

        if (config.signal) {
            config.signal.addEventListener('abort', () => {
                ws.close();
            });
        }

        ws.on('message', (data, isBinary) => {
            try {
                if (!isBinary || typeof data === 'string' || (Buffer.isBuffer(data) && !handshakeComplete)) {
                    // Text frame — AWPP control message
                    const text = Buffer.isBuffer(data) ? data.toString('utf8') : data;
                    const msg = JSON.parse(text);

                    if (msg.type === 'challenge') {
                        // Step 2: Send registration
                        const reg = {
                            type: 'register',
                            protocol: 'awpp/1',
                            transport: 'websocket',
                            name: config.name,
                            version: config.version || '1.0.0',
                            public_key: signer.publicKeyFormatted,
                            challenge_signature: signer.signChallenge(msg.nonce),
                            oauth_token: null,
                            capabilities: ['batch'],
                            max_batch_size: 1000,
                            transport_codec: codecName,
                            requested_pipelines: config.pipelines || [],
                            binding: 'dedicated',
                        };
                        ws.send(JSON.stringify(reg));
                    } else if (msg.type === 'accepted') {
                        sessionId = msg.session_id;
                        heartbeatInterval = msg.heartbeat_interval_ms || 10000;
                        batchSigningEnabled = msg.batch_signing || false;
                        handshakeComplete = true;

                        // Start heartbeat
                        heartbeatTimer = setInterval(sendHeartbeat, heartbeatInterval);

                        // Update codec if server negotiated different
                        if (msg.transport_codec && msg.transport_codec !== codecName) {
                            codec.name = msg.transport_codec;
                        }
                    } else if (msg.type === 'rejected') {
                        ws.close();
                        reject(new Error(`AWPP rejected: [${msg.code}] ${msg.message}`));
                    } else if (msg.type === 'drain') {
                        ws.close();
                    } else if (msg.type === 'heartbeat') {
                        // Server heartbeat — acknowledged by our heartbeat loop
                    }
                    return;
                }

                // Binary frame — batch data with routing header
                const buf = Buffer.isBuffer(data) ? data : Buffer.from(data);
                const { pipelineName, partition, batchData } = parseDataFrame(buf);

                const { batchId, events } = decodeBatchRequest(batchData, codec);

                // Process events
                let outputsPerEvent;
                if (batchProcessFn) {
                    outputsPerEvent = batchProcessFn(events);
                } else if (processFn) {
                    outputsPerEvent = events.map((event) => processFn(event));
                } else {
                    // Passthrough
                    outputsPerEvent = events.map(() => []);
                }

                // Encode response
                const responseBatch = encodeBatchResponse(
                    batchId,
                    outputsPerEvent,
                    codec,
                    signer,
                    batchSigningEnabled,
                );
                const responseFrame = buildDataFrame(pipelineName, partition, responseBatch);
                ws.send(responseFrame);

            } catch (err) {
                console.error('Aeon SDK: error processing message:', err);
            }
        });
    });
}

// ── Exports ────────────────────────────────────────────────────────────

module.exports = {
    // Types
    CODEC_MSGPACK,
    CODEC_JSON,

    // Codec
    Codec,

    // Auth
    Signer,

    // Wire format
    crc32,
    decodeBatchRequest,
    encodeBatchResponse,
    buildDataFrame,
    parseDataFrame,

    // Processor registration
    processor,
    batchProcessor,
    outputWithIdentity,

    // Runner
    run,
};
