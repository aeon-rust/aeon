package aeon

import (
	"bytes"
	"crypto/ed25519"
	"encoding/base64"
	"encoding/binary"
	"encoding/hex"
	"fmt"
	"hash/crc32"
	"strings"
	"testing"
)

// ── Codec Tests ──────────────────────────────────────────────────────────

func TestMsgPackCodecEventRoundtrip(t *testing.T) {
	codec := &Codec{Name: CodecMsgPack}
	event := Event{
		ID:        "550e8400-e29b-41d4-a716-446655440000",
		Timestamp: 1000000,
		Source:    "test-source",
		Partition: 3,
		Metadata:  [][]string{{"key1", "val1"}},
		Payload:   []byte("hello world"),
	}

	encoded, err := codec.EncodeEvent(event)
	if err != nil {
		t.Fatalf("encode: %v", err)
	}

	decoded, err := codec.DecodeEvent(encoded)
	if err != nil {
		t.Fatalf("decode: %v", err)
	}

	if decoded.ID != event.ID {
		t.Errorf("ID: got %q, want %q", decoded.ID, event.ID)
	}
	if decoded.Source != event.Source {
		t.Errorf("Source: got %q, want %q", decoded.Source, event.Source)
	}
	if decoded.Partition != event.Partition {
		t.Errorf("Partition: got %d, want %d", decoded.Partition, event.Partition)
	}
	if !bytes.Equal(decoded.Payload, event.Payload) {
		t.Errorf("Payload: got %q, want %q", decoded.Payload, event.Payload)
	}
}

func TestJSONCodecEventRoundtrip(t *testing.T) {
	codec := &Codec{Name: CodecJSON}
	event := Event{
		ID:        "test-uuid",
		Timestamp: 42,
		Source:    "json-src",
		Partition: 0,
		Metadata:  [][]string{},
		Payload:   []byte("data"),
	}

	encoded, err := codec.EncodeEvent(event)
	if err != nil {
		t.Fatalf("encode: %v", err)
	}

	decoded, err := codec.DecodeEvent(encoded)
	if err != nil {
		t.Fatalf("decode: %v", err)
	}

	if decoded.Source != "json-src" {
		t.Errorf("Source: got %q, want %q", decoded.Source, "json-src")
	}
}

func TestMsgPackCodecOutputRoundtrip(t *testing.T) {
	codec := &Codec{Name: CodecMsgPack}
	eventID := "some-uuid"
	partition := 5
	out := Output{
		Destination:     "sink-topic",
		Payload:         []byte("output-data"),
		Key:             []byte("my-key"),
		Headers:         [][]string{{"h1", "v1"}, {"h2", "v2"}},
		SourceEventID:   &eventID,
		SourcePartition: &partition,
	}

	encoded, err := codec.EncodeOutput(out)
	if err != nil {
		t.Fatalf("encode: %v", err)
	}

	decoded, err := codec.DecodeOutput(encoded)
	if err != nil {
		t.Fatalf("decode: %v", err)
	}

	if decoded.Destination != "sink-topic" {
		t.Errorf("Destination: got %q, want %q", decoded.Destination, "sink-topic")
	}
	if !bytes.Equal(decoded.Payload, []byte("output-data")) {
		t.Errorf("Payload mismatch")
	}
	if !bytes.Equal(decoded.Key, []byte("my-key")) {
		t.Errorf("Key mismatch")
	}
	if *decoded.SourceEventID != eventID {
		t.Errorf("SourceEventID: got %q, want %q", *decoded.SourceEventID, eventID)
	}
}

func TestMsgPackMoreCompactThanJSON(t *testing.T) {
	mp := &Codec{Name: CodecMsgPack}
	js := &Codec{Name: CodecJSON}
	event := Event{
		ID:        "550e8400-e29b-41d4-a716-446655440000",
		Timestamp: 1234567890,
		Source:    "a-source",
		Partition: 1,
		Metadata:  [][]string{{"content-type", "application/json"}},
		Payload:   []byte(`{"key": "value", "number": 42}`),
	}

	mpBytes, _ := mp.EncodeEvent(event)
	jsBytes, _ := js.EncodeEvent(event)

	if len(mpBytes) >= len(jsBytes) {
		t.Errorf("msgpack (%d bytes) should be smaller than json (%d bytes)", len(mpBytes), len(jsBytes))
	}
}

// ── Data Frame Tests ─────────────────────────────────────────────────────

func TestDataFrameRoundtrip(t *testing.T) {
	pipeline := "my-pipeline"
	partition := uint16(7)
	batchData := []byte("some-batch-wire-data")

	frame := BuildDataFrame(pipeline, partition, batchData)
	name, part, data, err := ParseDataFrame(frame)
	if err != nil {
		t.Fatalf("parse: %v", err)
	}

	if name != pipeline {
		t.Errorf("name: got %q, want %q", name, pipeline)
	}
	if part != partition {
		t.Errorf("partition: got %d, want %d", part, partition)
	}
	if !bytes.Equal(data, batchData) {
		t.Errorf("data mismatch")
	}
}

func TestDataFrameEmptyName(t *testing.T) {
	frame := BuildDataFrame("", 0, []byte("data"))
	name, part, data, err := ParseDataFrame(frame)
	if err != nil {
		t.Fatalf("parse: %v", err)
	}
	if name != "" || part != 0 || !bytes.Equal(data, []byte("data")) {
		t.Errorf("unexpected result: name=%q part=%d data=%q", name, part, data)
	}
}

func TestDataFrameTooShort(t *testing.T) {
	_, _, _, err := ParseDataFrame(make([]byte, 5))
	if err == nil {
		t.Error("expected error for short frame")
	}
}

func TestDataFrameTruncatedName(t *testing.T) {
	frame := make([]byte, 8)
	binary.LittleEndian.PutUint32(frame[0:4], 100) // name_len = 100 but only 4 bytes follow
	_, _, _, err := ParseDataFrame(frame)
	if err == nil {
		t.Error("expected error for truncated name")
	}
}

// ── Batch Wire Format Tests ──────────────────────────────────────────────

func buildBatchRequest(batchID uint64, eventsBytes [][]byte) []byte {
	var buf []byte

	var batchIDBytes [8]byte
	binary.LittleEndian.PutUint64(batchIDBytes[:], batchID)
	buf = append(buf, batchIDBytes[:]...)

	var countBytes [4]byte
	binary.LittleEndian.PutUint32(countBytes[:], uint32(len(eventsBytes)))
	buf = append(buf, countBytes[:]...)

	for _, eb := range eventsBytes {
		var lenBytes [4]byte
		binary.LittleEndian.PutUint32(lenBytes[:], uint32(len(eb)))
		buf = append(buf, lenBytes[:]...)
		buf = append(buf, eb...)
	}

	crcVal := crc32.ChecksumIEEE(buf)
	var crcBytes [4]byte
	binary.LittleEndian.PutUint32(crcBytes[:], crcVal)
	buf = append(buf, crcBytes[:]...)

	return buf
}

func TestDecodeBatchRequestMsgPack(t *testing.T) {
	codec := &Codec{Name: CodecMsgPack}
	event := Event{
		ID: "test-uuid", Timestamp: 999, Source: "src", Partition: 0,
		Metadata: [][]string{}, Payload: []byte("hello"),
	}
	eventBytes, _ := codec.EncodeEvent(event)
	wire := buildBatchRequest(42, [][]byte{eventBytes})

	batchID, events, err := DecodeBatchRequest(wire, codec)
	if err != nil {
		t.Fatalf("decode: %v", err)
	}
	if batchID != 42 {
		t.Errorf("batchID: got %d, want 42", batchID)
	}
	if len(events) != 1 {
		t.Fatalf("events: got %d, want 1", len(events))
	}
	if events[0].ID != "test-uuid" {
		t.Errorf("event ID: got %q", events[0].ID)
	}
	if !bytes.Equal(events[0].Payload, []byte("hello")) {
		t.Errorf("payload: got %q", events[0].Payload)
	}
}

func TestDecodeBatchRequestCRCMismatch(t *testing.T) {
	codec := &Codec{Name: CodecMsgPack}
	event := Event{ID: "x", Source: "s", Metadata: [][]string{}, Payload: []byte{}}
	eventBytes, _ := codec.EncodeEvent(event)
	wire := buildBatchRequest(1, [][]byte{eventBytes})

	// Corrupt first byte
	corrupted := make([]byte, len(wire))
	copy(corrupted, wire)
	corrupted[0] ^= 0xFF

	_, _, err := DecodeBatchRequest(corrupted, codec)
	if err == nil {
		t.Error("expected CRC error")
	}
}

func TestEncodeBatchResponseStructure(t *testing.T) {
	codec := &Codec{Name: CodecMsgPack}
	out := Output{Destination: "dest", Payload: []byte("result"), Headers: [][]string{}}
	var sig [64]byte

	wire, err := EncodeBatchResponse(7, [][]Output{{out}}, sig, codec)
	if err != nil {
		t.Fatalf("encode: %v", err)
	}

	if len(wire) < 80 {
		t.Errorf("wire too short: %d bytes", len(wire))
	}

	batchID := binary.LittleEndian.Uint64(wire[0:8])
	if batchID != 7 {
		t.Errorf("batchID: got %d, want 7", batchID)
	}

	eventCount := binary.LittleEndian.Uint32(wire[8:12])
	if eventCount != 1 {
		t.Errorf("eventCount: got %d, want 1", eventCount)
	}

	// Last 64 bytes = signature
	if !bytes.Equal(wire[len(wire)-64:], sig[:]) {
		t.Error("signature mismatch")
	}
}

func TestBatchRequestMultipleEvents(t *testing.T) {
	codec := &Codec{Name: CodecMsgPack}
	var eventsBytes [][]byte
	for i := 0; i < 5; i++ {
		event := Event{
			ID: fmt.Sprintf("evt-%d", i), Source: "s", Metadata: [][]string{},
			Payload: []byte(fmt.Sprintf("payload-%d", i)),
		}
		eb, _ := codec.EncodeEvent(event)
		eventsBytes = append(eventsBytes, eb)
	}
	wire := buildBatchRequest(123, eventsBytes)

	batchID, events, err := DecodeBatchRequest(wire, codec)
	if err != nil {
		t.Fatalf("decode: %v", err)
	}
	if batchID != 123 {
		t.Errorf("batchID: got %d", batchID)
	}
	if len(events) != 5 {
		t.Fatalf("events: got %d", len(events))
	}
	for i, e := range events {
		expected := fmt.Sprintf("evt-%d", i)
		if e.ID != expected {
			t.Errorf("event %d ID: got %q, want %q", i, e.ID, expected)
		}
	}
}

// ── ED25519 Signer Tests ────────────────────────────────────────────────

func TestSignerGenerate(t *testing.T) {
	signer, err := GenerateSigner()
	if err != nil {
		t.Fatalf("generate: %v", err)
	}

	rawHex := signer.PublicKeyHex()
	if len(rawHex) != 64 {
		t.Errorf("public key hex: got %d chars, want 64", len(rawHex))
	}

	fp := signer.Fingerprint()
	if len(fp) != 64 {
		t.Errorf("fingerprint: got %d chars, want 64", len(fp))
	}
}

func TestSignerAWPPPublicKey(t *testing.T) {
	// The AWPP public key must be "ed25519:<base64>" — this is the
	// form the Aeon identity store keys on. Both WS and WT Register
	// messages send this, not the raw hex.
	signer, _ := GenerateSigner()
	pk := signer.AWPPPublicKey()

	if !strings.HasPrefix(pk, "ed25519:") {
		t.Fatalf("AWPPPublicKey: got %q, expected ed25519: prefix", pk)
	}
	b64 := strings.TrimPrefix(pk, "ed25519:")
	decoded, err := base64.StdEncoding.DecodeString(b64)
	if err != nil {
		t.Fatalf("base64 decode: %v", err)
	}
	if len(decoded) != 32 {
		t.Errorf("decoded key len: got %d, want 32", len(decoded))
	}
	if !bytes.Equal(decoded, signer.publicKey) {
		t.Error("decoded AWPPPublicKey doesn't match raw public key")
	}
}

func TestSignerChallengeResponse(t *testing.T) {
	// The server builds a hex-encoded random nonce, and
	// processor_auth::verify_challenge hex-decodes it before verifying
	// the ED25519 signature against the raw bytes. So SignChallenge
	// must hex-decode the nonce first and sign the raw bytes — signing
	// the UTF-8 bytes of the hex string would fail verification on
	// the server side.
	signer, _ := GenerateSigner()
	nonceHex := strings.Repeat("0123456789abcdef", 4) // 32 raw bytes → 64 hex chars
	sigHex, err := signer.SignChallenge(nonceHex)
	if err != nil {
		t.Fatalf("SignChallenge: %v", err)
	}

	if len(sigHex) != 128 {
		t.Errorf("signature hex: got %d chars, want 128", len(sigHex))
	}

	nonceBytes, err := hex.DecodeString(nonceHex)
	if err != nil {
		t.Fatalf("hex decode nonce: %v", err)
	}
	sigBytes, err := hex.DecodeString(sigHex)
	if err != nil {
		t.Fatalf("hex decode sig: %v", err)
	}
	if !ed25519.Verify(signer.publicKey, nonceBytes, sigBytes) {
		t.Error("signature verification failed (should verify against raw nonce bytes)")
	}
}

func TestSignerChallengeRejectsInvalidHex(t *testing.T) {
	signer, _ := GenerateSigner()
	if _, err := signer.SignChallenge("not-valid-hex!"); err == nil {
		t.Error("SignChallenge accepted invalid hex")
	}
}

func TestSignerBatchSigning(t *testing.T) {
	signer, _ := GenerateSigner()
	data := []byte("batch-data-to-sign")
	sig := signer.SignBatch(data)

	if !ed25519.Verify(signer.publicKey, data, sig[:]) {
		t.Error("batch signature verification failed")
	}
}

func TestSignerFingerprintDeterministic(t *testing.T) {
	signer, _ := GenerateSigner()
	fp1 := signer.Fingerprint()
	fp2 := signer.Fingerprint()
	if fp1 != fp2 {
		t.Errorf("fingerprint not deterministic: %q != %q", fp1, fp2)
	}
}

// ── Output Builder Tests ────────────────────────────────────────────────

func TestOutputWithEventIdentity(t *testing.T) {
	offset := int64(42)
	event := Event{
		ID:           "evt-123",
		Partition:    5,
		SourceOffset: &offset,
	}
	out := &Output{Destination: "d", Payload: []byte{}}
	out.WithEventIdentity(event)

	if *out.SourceEventID != "evt-123" {
		t.Errorf("SourceEventID: got %q", *out.SourceEventID)
	}
	if *out.SourcePartition != 5 {
		t.Errorf("SourcePartition: got %d", *out.SourcePartition)
	}
	if *out.SourceOffset != 42 {
		t.Errorf("SourceOffset: got %d", *out.SourceOffset)
	}
}
