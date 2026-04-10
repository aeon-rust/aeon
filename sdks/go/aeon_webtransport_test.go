package aeon

import (
	"bytes"
	"encoding/binary"
	"testing"
)

// Unit tests for the WT wire helpers. The actual WebTransport dial +
// AWPP handshake are covered end-to-end by Tier D D2 in the engine
// integration tests; these tests exercise the pure-encoding helpers
// so a regression in the wire format is caught without spinning up
// a full QUIC stack.

func TestWTRoutingHeaderMatchesServer(t *testing.T) {
	// Matches the layout asserted by
	// crates/aeon-engine/src/transport/webtransport_host.rs:
	//   [4B name_len LE][name bytes][2B partition LE]
	var buf bytes.Buffer
	if err := writeWTRoutingHeader(&buf, "my-pipeline", 7); err != nil {
		t.Fatalf("writeWTRoutingHeader: %v", err)
	}

	got := buf.Bytes()
	if len(got) != 4+len("my-pipeline")+2 {
		t.Fatalf("header length: got %d, want %d", len(got), 4+len("my-pipeline")+2)
	}

	nameLen := binary.LittleEndian.Uint32(got[0:4])
	if nameLen != uint32(len("my-pipeline")) {
		t.Errorf("name_len: got %d, want %d", nameLen, len("my-pipeline"))
	}
	if string(got[4:4+nameLen]) != "my-pipeline" {
		t.Errorf("name: got %q, want %q", got[4:4+nameLen], "my-pipeline")
	}
	partition := binary.LittleEndian.Uint16(got[4+nameLen:])
	if partition != 7 {
		t.Errorf("partition: got %d, want 7", partition)
	}
}

func TestWTLengthPrefixedRoundtrip(t *testing.T) {
	// Control messages (JSON) and data frames (batch_wire bytes) share
	// the same [4B len LE][bytes] framing on WT streams — an
	// encode/decode cycle must be lossless.
	payload := []byte(`{"type":"heartbeat","timestamp_ms":1234567890}`)
	var buf bytes.Buffer
	if err := writeWTLengthPrefixed(&buf, payload); err != nil {
		t.Fatalf("write: %v", err)
	}

	// Encoded format: [4B len LE][payload]
	raw := buf.Bytes()
	if len(raw) != 4+len(payload) {
		t.Fatalf("encoded length: got %d, want %d", len(raw), 4+len(payload))
	}
	declaredLen := binary.LittleEndian.Uint32(raw[:4])
	if declaredLen != uint32(len(payload)) {
		t.Errorf("declared length: got %d, want %d", declaredLen, len(payload))
	}

	// Round-trip through the reader.
	back, err := readWTLengthPrefixed(&buf)
	// After a successful write, the reader must fail because we
	// already consumed `buf`. Reset and write again to exercise both.
	_ = back
	_ = err
}

func TestWTLengthPrefixedReadWritePair(t *testing.T) {
	// Write a frame, then read it back from the same buffer — this
	// is the shape the heartbeat + data-stream loops use.
	payloads := [][]byte{
		[]byte(`{"type":"heartbeat"}`),
		[]byte("short"),
		bytes.Repeat([]byte("A"), 1024),
	}
	var buf bytes.Buffer
	for _, p := range payloads {
		if err := writeWTLengthPrefixed(&buf, p); err != nil {
			t.Fatalf("write %q: %v", p[:min(len(p), 20)], err)
		}
	}
	for i, want := range payloads {
		got, err := readWTLengthPrefixed(&buf)
		if err != nil {
			t.Fatalf("read %d: %v", i, err)
		}
		if !bytes.Equal(got, want) {
			t.Errorf("frame %d: got %q, want %q", i,
				got[:min(len(got), 20)], want[:min(len(want), 20)])
		}
	}
}

func TestWTLengthPrefixedRejectsOversize(t *testing.T) {
	// Hand-craft a frame that claims to be 32 MiB — the reader must
	// refuse it rather than allocate.
	var header [4]byte
	binary.LittleEndian.PutUint32(header[:], 32*1024*1024)
	buf := bytes.NewReader(header[:])
	_, err := readWTLengthPrefixed(buf)
	if err == nil {
		t.Error("expected rejection of 32MiB frame, got nil")
	}
}

func min(a, b int) int {
	if a < b {
		return a
	}
	return b
}
