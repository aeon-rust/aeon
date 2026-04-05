// Package aeon provides the Aeon Processor SDK for Go.
//
// Connects to the Aeon engine via T4 WebSocket transport, runs the AWPP
// handshake (ED25519 challenge-response authentication), receives batch
// requests, calls the user's processor function, and sends batch responses.
//
// Usage:
//
//	import aeon "github.com/aeon-rust/aeon/sdks/go"
//
//	func main() {
//	    aeon.Run(aeon.Config{
//	        URL:       "ws://localhost:4471/api/v1/processors/connect",
//	        Name:      "my-processor",
//	        Version:   "1.0.0",
//	        Pipelines: []string{"my-pipeline"},
//	        Processor: func(event aeon.Event) []aeon.Output {
//	            return []aeon.Output{{Destination: "out", Payload: event.Payload}}
//	        },
//	    })
//	}
package aeon

import (
	"context"
	"crypto/ed25519"
	"crypto/rand"
	"crypto/sha256"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"hash/crc32"
	"log"
	"os"
	"sync"
	"time"

	"github.com/gorilla/websocket"
	"github.com/vmihailenco/msgpack/v5"
)

// ── Types ────────────────────────────────────────────────────────────────

// Event received from the Aeon engine over AWPP.
// Fields mirror the Rust WireEvent struct.
type Event struct {
	ID           string     `json:"id" msgpack:"id"`
	Timestamp    int64      `json:"timestamp" msgpack:"timestamp"`
	Source       string     `json:"source" msgpack:"source"`
	Partition    int        `json:"partition" msgpack:"partition"`
	Metadata     [][]string `json:"metadata" msgpack:"metadata"`
	Payload      []byte     `json:"payload" msgpack:"payload"`
	SourceOffset *int64     `json:"source_offset,omitempty" msgpack:"source_offset,omitempty"`
}

// Output to send back to the Aeon engine.
// Fields mirror the Rust WireOutput struct.
type Output struct {
	Destination     string     `json:"destination" msgpack:"destination"`
	Key             []byte     `json:"key,omitempty" msgpack:"key,omitempty"`
	Payload         []byte     `json:"payload" msgpack:"payload"`
	Headers         [][]string `json:"headers" msgpack:"headers"`
	SourceEventID   *string    `json:"source_event_id,omitempty" msgpack:"source_event_id,omitempty"`
	SourcePartition *int       `json:"source_partition,omitempty" msgpack:"source_partition,omitempty"`
	SourceOffset    *int64     `json:"source_offset,omitempty" msgpack:"source_offset,omitempty"`
}

// WithEventIdentity sets delivery tracking fields from the source event.
func (o *Output) WithEventIdentity(e Event) *Output {
	o.SourceEventID = &e.ID
	o.SourcePartition = &e.Partition
	o.SourceOffset = e.SourceOffset
	return o
}

// ProcessorFunc processes a single event and returns outputs.
type ProcessorFunc func(event Event) []Output

// BatchProcessorFunc processes a batch of events and returns outputs per event.
type BatchProcessorFunc func(events []Event) [][]Output

// ── Codec ────────────────────────────────────────────────────────────────

// CodecName identifies the transport codec.
type CodecName string

const (
	CodecMsgPack CodecName = "msgpack"
	CodecJSON    CodecName = "json"
)

// Codec handles encoding/decoding of WireEvent and WireOutput.
type Codec struct {
	Name CodecName
}

// EncodeEvent serializes an Event.
func (c *Codec) EncodeEvent(e Event) ([]byte, error) {
	if c.Name == CodecMsgPack {
		return msgpack.Marshal(e)
	}
	return json.Marshal(e)
}

// DecodeEvent deserializes an Event.
func (c *Codec) DecodeEvent(data []byte) (Event, error) {
	var e Event
	if c.Name == CodecMsgPack {
		err := msgpack.Unmarshal(data, &e)
		return e, err
	}
	err := json.Unmarshal(data, &e)
	return e, err
}

// EncodeOutput serializes an Output.
func (c *Codec) EncodeOutput(o Output) ([]byte, error) {
	if c.Name == CodecMsgPack {
		return msgpack.Marshal(o)
	}
	return json.Marshal(o)
}

// DecodeOutput deserializes an Output.
func (c *Codec) DecodeOutput(data []byte) (Output, error) {
	var o Output
	if c.Name == CodecMsgPack {
		err := msgpack.Unmarshal(data, &o)
		return o, err
	}
	err := json.Unmarshal(data, &o)
	return o, err
}

// ── Batch Wire Format ────────────────────────────────────────────────────

// DecodeBatchRequest parses a batch request from wire format.
//
// Wire: [8B batch_id LE][4B event_count LE][per event: 4B len LE + bytes][4B CRC32]
func DecodeBatchRequest(data []byte, codec *Codec) (batchID uint64, events []Event, err error) {
	if len(data) < 16 {
		return 0, nil, fmt.Errorf("batch request too short: %d bytes", len(data))
	}

	// Verify CRC32
	payload := data[:len(data)-4]
	expectedCRC := binary.LittleEndian.Uint32(data[len(data)-4:])
	actualCRC := crc32.ChecksumIEEE(payload)
	if expectedCRC != actualCRC {
		return 0, nil, fmt.Errorf("CRC32 mismatch: expected %#x, got %#x", expectedCRC, actualCRC)
	}

	batchID = binary.LittleEndian.Uint64(data[0:8])
	eventCount := binary.LittleEndian.Uint32(data[8:12])

	offset := 12
	events = make([]Event, 0, eventCount)

	for i := uint32(0); i < eventCount; i++ {
		if offset+4 > len(payload) {
			return 0, nil, fmt.Errorf("unexpected end of batch request at event %d", i)
		}
		eLen := int(binary.LittleEndian.Uint32(data[offset : offset+4]))
		offset += 4
		if offset+eLen > len(payload) {
			return 0, nil, fmt.Errorf("event %d data exceeds buffer", i)
		}
		event, err := codec.DecodeEvent(data[offset : offset+eLen])
		if err != nil {
			return 0, nil, fmt.Errorf("decode event %d: %w", i, err)
		}
		events = append(events, event)
		offset += eLen
	}

	return batchID, events, nil
}

// EncodeBatchResponse creates a batch response in wire format.
//
// Wire: [8B batch_id LE][4B event_count LE]
//
//	[per event: 4B output_count LE, per output: 4B len LE + bytes]
//	[4B CRC32][64B signature]
func EncodeBatchResponse(batchID uint64, outputsPerEvent [][]Output, signature [64]byte, codec *Codec) ([]byte, error) {
	var buf []byte

	// batch_id
	var batchIDBytes [8]byte
	binary.LittleEndian.PutUint64(batchIDBytes[:], batchID)
	buf = append(buf, batchIDBytes[:]...)

	// event_count
	var countBytes [4]byte
	binary.LittleEndian.PutUint32(countBytes[:], uint32(len(outputsPerEvent)))
	buf = append(buf, countBytes[:]...)

	for _, outputs := range outputsPerEvent {
		// output_count
		binary.LittleEndian.PutUint32(countBytes[:], uint32(len(outputs)))
		buf = append(buf, countBytes[:]...)

		for _, out := range outputs {
			encoded, err := codec.EncodeOutput(out)
			if err != nil {
				return nil, fmt.Errorf("encode output: %w", err)
			}
			var lenBytes [4]byte
			binary.LittleEndian.PutUint32(lenBytes[:], uint32(len(encoded)))
			buf = append(buf, lenBytes[:]...)
			buf = append(buf, encoded...)
		}
	}

	// CRC32
	crcVal := crc32.ChecksumIEEE(buf)
	var crcBytes [4]byte
	binary.LittleEndian.PutUint32(crcBytes[:], crcVal)
	buf = append(buf, crcBytes[:]...)

	// Signature
	buf = append(buf, signature[:]...)

	return buf, nil
}

// ── Data Frame ───────────────────────────────────────────────────────────

// BuildDataFrame creates a T4 binary data frame with routing header.
//
// Format: [4B name_len LE][name][2B partition LE][batch_wire data]
func BuildDataFrame(pipelineName string, partition uint16, batchWireData []byte) []byte {
	nameBytes := []byte(pipelineName)
	buf := make([]byte, 4+len(nameBytes)+2+len(batchWireData))

	binary.LittleEndian.PutUint32(buf[0:4], uint32(len(nameBytes)))
	copy(buf[4:], nameBytes)
	binary.LittleEndian.PutUint16(buf[4+len(nameBytes):], partition)
	copy(buf[4+len(nameBytes)+2:], batchWireData)

	return buf
}

// ParseDataFrame parses a T4 binary data frame routing header.
//
// Returns (pipeline_name, partition, batch_wire_data, error).
func ParseDataFrame(data []byte) (string, uint16, []byte, error) {
	if len(data) < 7 {
		return "", 0, nil, fmt.Errorf("data frame too short: %d bytes", len(data))
	}
	nameLen := int(binary.LittleEndian.Uint32(data[0:4]))
	if len(data) < 4+nameLen+2 {
		return "", 0, nil, fmt.Errorf("data frame name truncated")
	}
	name := string(data[4 : 4+nameLen])
	partition := binary.LittleEndian.Uint16(data[4+nameLen : 4+nameLen+2])
	batchData := data[4+nameLen+2:]
	return name, partition, batchData, nil
}

// ── ED25519 Signer ──────────────────────────────────────────────────────

// Signer holds an ED25519 key pair for AWPP authentication and batch signing.
type Signer struct {
	privateKey ed25519.PrivateKey
	publicKey  ed25519.PublicKey
}

// NewSigner creates a signer from a 64-byte ED25519 private key.
func NewSigner(privateKey ed25519.PrivateKey) *Signer {
	return &Signer{
		privateKey: privateKey,
		publicKey:  privateKey.Public().(ed25519.PublicKey),
	}
}

// NewSignerFromSeed creates a signer from a 32-byte ED25519 seed.
func NewSignerFromSeed(seed []byte) (*Signer, error) {
	if len(seed) != ed25519.SeedSize {
		return nil, fmt.Errorf("expected %d-byte seed, got %d", ed25519.SeedSize, len(seed))
	}
	pk := ed25519.NewKeyFromSeed(seed)
	return NewSigner(pk), nil
}

// GenerateSigner creates a signer with a new random key pair.
func GenerateSigner() (*Signer, error) {
	_, priv, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		return nil, fmt.Errorf("generate ED25519 key: %w", err)
	}
	return NewSigner(priv), nil
}

// LoadSigner loads a signer from a 32-byte seed file.
func LoadSigner(path string) (*Signer, error) {
	seed, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("read key file: %w", err)
	}
	return NewSignerFromSeed(seed)
}

// PublicKeyHex returns the hex-encoded public key.
func (s *Signer) PublicKeyHex() string {
	return hex.EncodeToString(s.publicKey)
}

// Fingerprint returns the SHA-256 hex fingerprint of the public key.
func (s *Signer) Fingerprint() string {
	hash := sha256.Sum256(s.publicKey)
	return hex.EncodeToString(hash[:])
}

// SignChallenge signs an AWPP challenge nonce, returning the hex-encoded signature.
func (s *Signer) SignChallenge(nonce string) string {
	sig := ed25519.Sign(s.privateKey, []byte(nonce))
	return hex.EncodeToString(sig)
}

// SignBatch signs batch response data, returning the 64-byte signature.
func (s *Signer) SignBatch(data []byte) [64]byte {
	sig := ed25519.Sign(s.privateKey, data)
	var result [64]byte
	copy(result[:], sig)
	return result
}

// ── AWPP Protocol ────────────────────────────────────────────────────────

// controlMessage is the JSON envelope for AWPP control messages.
type controlMessage map[string]interface{}

func awppHandshake(conn *websocket.Conn, signer *Signer, cfg Config) (map[string]interface{}, error) {
	// Step 1: Receive Challenge
	_, raw, err := conn.ReadMessage()
	if err != nil {
		return nil, fmt.Errorf("read challenge: %w", err)
	}
	var challenge controlMessage
	if err := json.Unmarshal(raw, &challenge); err != nil {
		return nil, fmt.Errorf("parse challenge: %w", err)
	}
	if challenge["type"] != "challenge" {
		return nil, fmt.Errorf("expected challenge, got: %v", challenge["type"])
	}
	nonce, _ := challenge["nonce"].(string)
	log.Printf("[aeon] received AWPP challenge (protocol=%v)", challenge["protocol"])

	// Step 2: Send Register
	codecName := cfg.Codec
	if codecName == "" {
		codecName = string(CodecMsgPack)
	}
	register := controlMessage{
		"type":                "register",
		"protocol":            "awpp/1",
		"transport":           "websocket",
		"name":                cfg.Name,
		"version":             cfg.Version,
		"public_key":          signer.PublicKeyHex(),
		"challenge_signature": signer.SignChallenge(nonce),
		"capabilities":        []string{"batch"},
		"transport_codec":     codecName,
		"requested_pipelines": cfg.Pipelines,
		"binding":             "dedicated",
	}
	regJSON, _ := json.Marshal(register)
	if err := conn.WriteMessage(websocket.TextMessage, regJSON); err != nil {
		return nil, fmt.Errorf("send register: %w", err)
	}
	log.Printf("[aeon] sent AWPP register (name=%s, codec=%s)", cfg.Name, codecName)

	// Step 3: Receive Accepted or Rejected
	_, raw, err = conn.ReadMessage()
	if err != nil {
		return nil, fmt.Errorf("read accepted: %w", err)
	}
	var response controlMessage
	if err := json.Unmarshal(raw, &response); err != nil {
		return nil, fmt.Errorf("parse response: %w", err)
	}

	if response["type"] == "rejected" {
		return nil, fmt.Errorf("AWPP rejected: [%v] %v", response["code"], response["message"])
	}
	if response["type"] != "accepted" {
		return nil, fmt.Errorf("unexpected AWPP message: %v", response["type"])
	}

	log.Printf("[aeon] AWPP accepted (session=%v)", response["session_id"])
	return response, nil
}

// ── Config ───────────────────────────────────────────────────────────────

// Config for the processor runner.
type Config struct {
	// WebSocket URL (e.g. ws://localhost:4471/api/v1/processors/connect)
	URL string
	// Processor name (must match registered identity in Aeon)
	Name string
	// Processor version
	Version string
	// Path to 32-byte ED25519 seed file (alternative to PrivateKey)
	PrivateKeyPath string
	// Raw 32-byte ED25519 seed (alternative to PrivateKeyPath)
	PrivateKey []byte
	// List of pipeline names to request
	Pipelines []string
	// Transport codec ("msgpack" or "json", default: "msgpack")
	Codec string
	// Per-event processor function (set one of Processor or BatchProcessor)
	Processor ProcessorFunc
	// Batch processor function
	BatchProcessor BatchProcessorFunc
}

// ── Runner ───────────────────────────────────────────────────────────────

// Run connects to Aeon and runs the processor until the connection closes.
func Run(cfg Config) error {
	return RunContext(context.Background(), cfg)
}

// RunContext connects to Aeon and runs the processor with context cancellation.
func RunContext(ctx context.Context, cfg Config) error {
	// Resolve signer
	var signer *Signer
	var err error
	if cfg.PrivateKeyPath != "" {
		signer, err = LoadSigner(cfg.PrivateKeyPath)
	} else if cfg.PrivateKey != nil {
		signer, err = NewSignerFromSeed(cfg.PrivateKey)
	} else {
		log.Println("[aeon] no private key provided — generating ephemeral key pair")
		signer, err = GenerateSigner()
	}
	if err != nil {
		return fmt.Errorf("signer init: %w", err)
	}

	log.Printf("[aeon] processor %s v%s (fingerprint=%s...)", cfg.Name, cfg.Version, signer.Fingerprint()[:16])

	// Connect
	log.Printf("[aeon] connecting to %s", cfg.URL)
	conn, _, err := websocket.DefaultDialer.DialContext(ctx, cfg.URL, nil)
	if err != nil {
		return fmt.Errorf("connect: %w", err)
	}
	defer conn.Close()

	// AWPP handshake
	accepted, err := awppHandshake(conn, signer, cfg)
	if err != nil {
		return fmt.Errorf("handshake: %w", err)
	}

	// Determine effective codec
	codecName := CodecMsgPack
	if tc, ok := accepted["transport_codec"].(string); ok && tc != "" {
		codecName = CodecName(tc)
	}
	codec := &Codec{Name: codecName}

	hbIntervalMs := 10000.0
	if v, ok := accepted["heartbeat_interval_ms"].(float64); ok {
		hbIntervalMs = v
	}
	batchSigning := true
	if v, ok := accepted["batch_signing"].(bool); ok {
		batchSigning = v
	}

	log.Printf("[aeon] session active (codec=%s, hb=%dms, signing=%v)",
		codecName, int(hbIntervalMs), batchSigning)

	// Start heartbeat goroutine
	var wsMu sync.Mutex
	hbCtx, hbCancel := context.WithCancel(ctx)
	defer hbCancel()

	go heartbeatLoop(hbCtx, conn, &wsMu, time.Duration(hbIntervalMs)*time.Millisecond)

	// Message loop
	return messageLoop(ctx, conn, &wsMu, codec, signer, batchSigning, cfg)
}

func heartbeatLoop(ctx context.Context, conn *websocket.Conn, mu *sync.Mutex, interval time.Duration) {
	ticker := time.NewTicker(interval)
	defer ticker.Stop()

	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			hb := controlMessage{
				"type":         "heartbeat",
				"timestamp_ms": time.Now().UnixMilli(),
			}
			data, _ := json.Marshal(hb)
			mu.Lock()
			err := conn.WriteMessage(websocket.TextMessage, data)
			mu.Unlock()
			if err != nil {
				return
			}
		}
	}
}

func messageLoop(ctx context.Context, conn *websocket.Conn, wsMu *sync.Mutex, codec *Codec, signer *Signer, batchSigning bool, cfg Config) error {
	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		default:
		}

		msgType, data, err := conn.ReadMessage()
		if err != nil {
			if websocket.IsCloseError(err, websocket.CloseNormalClosure, websocket.CloseGoingAway) {
				return nil
			}
			return fmt.Errorf("read message: %w", err)
		}

		switch msgType {
		case websocket.TextMessage:
			handleControlMessage(data)

		case websocket.BinaryMessage:
			response, err := handleDataFrame(data, codec, signer, batchSigning, cfg)
			if err != nil {
				log.Printf("[aeon] data frame error: %v", err)
				continue
			}
			wsMu.Lock()
			err = conn.WriteMessage(websocket.BinaryMessage, response)
			wsMu.Unlock()
			if err != nil {
				return fmt.Errorf("send response: %w", err)
			}
		}
	}
}

func handleControlMessage(data []byte) {
	var msg controlMessage
	if err := json.Unmarshal(data, &msg); err != nil {
		log.Printf("[aeon] malformed control message: %v", err)
		return
	}

	switch msg["type"] {
	case "drain":
		log.Printf("[aeon] drain received (reason=%v)", msg["reason"])
	case "error":
		log.Printf("[aeon] host error: [%v] %v", msg["code"], msg["message"])
	case "heartbeat":
		// Host heartbeat — acknowledged
	default:
		log.Printf("[aeon] unhandled control: %v", msg["type"])
	}
}

func handleDataFrame(data []byte, codec *Codec, signer *Signer, batchSigning bool, cfg Config) ([]byte, error) {
	pipelineName, partition, batchWire, err := ParseDataFrame(data)
	if err != nil {
		return nil, err
	}

	batchID, events, err := DecodeBatchRequest(batchWire, codec)
	if err != nil {
		return nil, fmt.Errorf("decode batch: %w", err)
	}

	// Process
	var outputsPerEvent [][]Output
	if cfg.BatchProcessor != nil {
		outputsPerEvent = cfg.BatchProcessor(events)
	} else if cfg.Processor != nil {
		outputsPerEvent = make([][]Output, len(events))
		for i, e := range events {
			outputsPerEvent[i] = cfg.Processor(e)
		}
	} else {
		return nil, fmt.Errorf("no processor configured")
	}

	// Sign
	var signature [64]byte
	if batchSigning {
		body := buildResponseBody(batchID, outputsPerEvent, codec)
		signature = signer.SignBatch(body)
	}

	responseWire, err := EncodeBatchResponse(batchID, outputsPerEvent, signature, codec)
	if err != nil {
		return nil, fmt.Errorf("encode response: %w", err)
	}

	return BuildDataFrame(pipelineName, partition, responseWire), nil
}

func buildResponseBody(batchID uint64, outputsPerEvent [][]Output, codec *Codec) []byte {
	var buf []byte
	var tmp [8]byte

	binary.LittleEndian.PutUint64(tmp[:], batchID)
	buf = append(buf, tmp[:8]...)

	binary.LittleEndian.PutUint32(tmp[:4], uint32(len(outputsPerEvent)))
	buf = append(buf, tmp[:4]...)

	for _, outputs := range outputsPerEvent {
		binary.LittleEndian.PutUint32(tmp[:4], uint32(len(outputs)))
		buf = append(buf, tmp[:4]...)
		for _, out := range outputs {
			encoded, _ := codec.EncodeOutput(out)
			binary.LittleEndian.PutUint32(tmp[:4], uint32(len(encoded)))
			buf = append(buf, tmp[:4]...)
			buf = append(buf, encoded...)
		}
	}

	return buf
}
