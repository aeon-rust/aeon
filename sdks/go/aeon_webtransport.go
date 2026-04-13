// T3 WebTransport (QUIC/HTTP3) client for the Aeon Processor SDK.
//
// Mirrors the canonical Rust reference implementation in
// `crates/aeon-processor-client/src/webtransport.rs`. The on-the-wire
// protocol is identical to T4 WebSocket — same AWPP handshake, same
// batch_wire format, same ED25519 challenge-response — only the
// transport changes:
//
//   - One long-lived *bidirectional* control stream carries the AWPP
//     handshake and heartbeats, length-prefixed JSON: [4B len LE][JSON].
//   - One long-lived *bidirectional* data stream per (pipeline, partition)
//     in the Accepted message. Each data stream is opened by the client,
//     which writes a routing header exactly once:
//         [4B name_len LE][name bytes][2B partition LE]
//     then enters a read→process→write loop of length-prefixed batch
//     wire frames: [4B len LE][batch_wire bytes].
//
// Follows the six-step pattern in docs/WT-SDK-INTEGRATION-PLAN.md §4.1
// verbatim. See the Rust reference for the authoritative implementation.

package aeon

import (
	"context"
	"crypto/tls"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"sync"
	"time"

	"github.com/quic-go/webtransport-go"
)

// ── Config ───────────────────────────────────────────────────────────────

// ConfigWT is the WebTransport-specific processor config. It mirrors
// Config for T4 but adds TLS knobs and drops the "ws://" URL assumption.
type ConfigWT struct {
	// WebTransport URL (e.g. "https://localhost:4472"). Must be https://.
	URL string
	// Processor name (must match a registered identity in Aeon).
	Name string
	// Processor version.
	Version string
	// Path to 32-byte ED25519 seed file (alternative to PrivateKey).
	PrivateKeyPath string
	// Raw 32-byte ED25519 seed (alternative to PrivateKeyPath).
	PrivateKey []byte
	// List of pipeline names to request.
	Pipelines []string
	// Transport codec ("msgpack" or "json", default "msgpack").
	Codec string
	// Per-event processor function (set one of Processor or BatchProcessor).
	Processor ProcessorFunc
	// Batch processor function.
	BatchProcessor BatchProcessorFunc
	// Insecure trusts any TLS cert. Use ONLY for tests against a
	// self-signed server (e.g. Tier D D2). Never set this in production.
	Insecure bool
	// ServerName overrides the TLS SNI. Useful when connecting by IP
	// against a cert whose SAN only covers "localhost".
	ServerName string
}

// ── Entry points ─────────────────────────────────────────────────────────

// RunWebTransport connects to an Aeon engine over T3 WebTransport, runs
// the AWPP handshake, and processes batches until the session closes.
//
// Blocks until the connection ends (graceful close, error, or ctx done).
func RunWebTransport(cfg ConfigWT) error {
	return RunWebTransportContext(context.Background(), cfg)
}

// RunWebTransportContext is RunWebTransport with explicit context control.
func RunWebTransportContext(ctx context.Context, cfg ConfigWT) error {
	// Resolve signer (same logic as the T4 Run path).
	var signer *Signer
	var err error
	switch {
	case cfg.PrivateKeyPath != "":
		signer, err = LoadSigner(cfg.PrivateKeyPath)
	case cfg.PrivateKey != nil:
		signer, err = NewSignerFromSeed(cfg.PrivateKey)
	default:
		log.Println("[aeon/wt] no private key provided — generating ephemeral key pair")
		signer, err = GenerateSigner()
	}
	if err != nil {
		return fmt.Errorf("wt signer init: %w", err)
	}

	log.Printf("[aeon/wt] processor %s v%s (fingerprint=%s...)",
		cfg.Name, cfg.Version, signer.Fingerprint()[:16])

	// Build TLS config. In production this uses the system roots; in
	// test-mode (Insecure=true) we skip verification and optionally
	// pin an SNI so self-signed "localhost" certs work when we dial
	// by IP address.
	tlsCfg := &tls.Config{}
	if cfg.Insecure {
		tlsCfg.InsecureSkipVerify = true
	}
	if cfg.ServerName != "" {
		tlsCfg.ServerName = cfg.ServerName
	}

	dialer := &webtransport.Dialer{
		TLSClientConfig: tlsCfg,
	}

	log.Printf("[aeon/wt] dialing %s", cfg.URL)
	_, session, err := dialer.Dial(ctx, cfg.URL, nil)
	if err != nil {
		return fmt.Errorf("wt dial: %w", err)
	}
	defer session.CloseWithError(0, "processor exiting")

	// Step 1-3: AWPP handshake on a fresh bidirectional control stream.
	ctrlStream, err := session.OpenStreamSync(ctx)
	if err != nil {
		return fmt.Errorf("open control stream: %w", err)
	}

	// webtransport-go only writes the WT stream header (frame type +
	// session id) on the first Write() call — not on OpenStreamSync. If
	// we try to read the challenge first, the server's accept_bi() never
	// fires because the peer hasn't seen anything on the stream, and the
	// whole handshake deadlocks until the QUIC idle timeout.
	//
	// Force the header out with an explicit empty write. quic-go's
	// Write([]byte{}) is a no-op on its own, but the webtransport-go
	// SendStream wrapper calls maybeSendStreamHeader() before delegating
	// to quic-go — so the header IS flushed as a side effect. See
	// webtransport-go/stream.go (SendStream.Write / maybeSendStreamHeader)
	// and quic-go/send_stream.go (early-return on len(p)==0).
	if _, err := ctrlStream.Write(nil); err != nil {
		return fmt.Errorf("flush control stream header: %w", err)
	}

	accepted, err := awppHandshakeWT(ctx, ctrlStream, signer, cfg)
	if err != nil {
		return fmt.Errorf("awpp handshake: %w", err)
	}

	// Determine effective codec from the Accepted message; fall back
	// to msgpack (the server default).
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

	log.Printf("[aeon/wt] session active (codec=%s, hb=%dms, signing=%v)",
		codecName, int(hbIntervalMs), batchSigning)

	// Step 4: spawn heartbeat on the control stream.
	hbCtx, hbCancel := context.WithCancel(ctx)
	defer hbCancel()

	var ctrlMu sync.Mutex
	go heartbeatLoopWT(hbCtx, ctrlStream, &ctrlMu, time.Duration(hbIntervalMs)*time.Millisecond)

	// Step 5: open one long-lived bidirectional data stream per
	// (pipeline, partition) assignment from Accepted.pipelines. Each
	// stream writes its routing header once, then loops on
	// length-prefixed batch frames until the server closes the stream
	// or ctx is cancelled.
	pipelinesRaw, _ := accepted["pipelines"].([]interface{})
	if len(pipelinesRaw) == 0 {
		log.Printf("[aeon/wt] warning: no pipeline assignments in Accepted — nothing to do")
	}

	streamCtx, streamCancel := context.WithCancel(ctx)
	defer streamCancel()
	var wg sync.WaitGroup
	errCh := make(chan error, 16)

	for _, entryRaw := range pipelinesRaw {
		entry, ok := entryRaw.(map[string]interface{})
		if !ok {
			continue
		}
		name, _ := entry["name"].(string)
		partitionsRaw, _ := entry["partitions"].([]interface{})
		for _, p := range partitionsRaw {
			partitionF, ok := p.(float64)
			if !ok {
				continue
			}
			partition := uint16(partitionF)
			pipelineName := name

			dataStream, err := session.OpenStreamSync(streamCtx)
			if err != nil {
				streamCancel()
				return fmt.Errorf("open data stream %s/%d: %w", pipelineName, partition, err)
			}
			if err := writeWTRoutingHeader(dataStream, pipelineName, partition); err != nil {
				streamCancel()
				return fmt.Errorf("write routing header %s/%d: %w", pipelineName, partition, err)
			}

			log.Printf("[aeon/wt] data stream registered pipeline=%s partition=%d",
				pipelineName, partition)

			wg.Add(1)
			go func(ds *webtransport.Stream, pname string, part uint16) {
				defer wg.Done()
				err := runWTDataStream(streamCtx, ds, codec, signer, batchSigning, cfg)
				if err != nil && err != io.EOF && streamCtx.Err() == nil {
					select {
					case errCh <- fmt.Errorf("data stream %s/%d: %w", pname, part, err):
					default:
					}
				}
				// First stream to end tears down the session — matches
				// the Rust reference's `join_next().await` semantics.
				streamCancel()
			}(dataStream, pipelineName, partition)
		}
	}

	wg.Wait()
	hbCancel()

	select {
	case err := <-errCh:
		return err
	default:
	}
	if ctx.Err() != nil {
		return ctx.Err()
	}
	log.Printf("[aeon/wt] processor disconnected cleanly")
	return nil
}

// ── AWPP handshake over WT ───────────────────────────────────────────────

func awppHandshakeWT(
	ctx context.Context,
	stream *webtransport.Stream,
	signer *Signer,
	cfg ConfigWT,
) (map[string]interface{}, error) {
	_ = ctx // stream reads/writes are context-aware via the session

	// Step 1: receive Challenge
	raw, err := readWTLengthPrefixed(stream)
	if err != nil {
		return nil, fmt.Errorf("read challenge: %w", err)
	}
	var challenge map[string]interface{}
	if err := json.Unmarshal(raw, &challenge); err != nil {
		return nil, fmt.Errorf("parse challenge: %w", err)
	}
	if challenge["type"] != "challenge" {
		return nil, fmt.Errorf("expected challenge, got: %v", challenge["type"])
	}
	nonce, _ := challenge["nonce"].(string)
	log.Printf("[aeon/wt] received AWPP challenge (protocol=%v)", challenge["protocol"])

	// Step 2: send Register
	codecName := cfg.Codec
	if codecName == "" {
		codecName = string(CodecMsgPack)
	}
	challengeSig, err := signer.SignChallenge(nonce)
	if err != nil {
		return nil, fmt.Errorf("sign challenge: %w", err)
	}
	register := map[string]interface{}{
		"type":                "register",
		"protocol":            "awpp/1",
		"transport":           "webtransport",
		"name":                cfg.Name,
		"version":             cfg.Version,
		"public_key":          signer.AWPPPublicKey(),
		"challenge_signature": challengeSig,
		"capabilities":        []string{"batch"},
		"transport_codec":     codecName,
		"requested_pipelines": cfg.Pipelines,
		"binding":             "dedicated",
	}
	regJSON, err := json.Marshal(register)
	if err != nil {
		return nil, fmt.Errorf("marshal register: %w", err)
	}
	if err := writeWTLengthPrefixed(stream, regJSON); err != nil {
		return nil, fmt.Errorf("send register: %w", err)
	}
	log.Printf("[aeon/wt] sent AWPP register (name=%s, codec=%s)", cfg.Name, codecName)

	// Step 3: receive Accepted or Rejected
	raw, err = readWTLengthPrefixed(stream)
	if err != nil {
		return nil, fmt.Errorf("read accepted: %w", err)
	}
	var response map[string]interface{}
	if err := json.Unmarshal(raw, &response); err != nil {
		return nil, fmt.Errorf("parse response: %w", err)
	}
	if response["type"] == "rejected" {
		return nil, fmt.Errorf("AWPP rejected: [%v] %v", response["code"], response["message"])
	}
	if response["type"] != "accepted" {
		return nil, fmt.Errorf("unexpected AWPP message: %v", response["type"])
	}

	log.Printf("[aeon/wt] AWPP accepted (session=%v)", response["session_id"])
	return response, nil
}

// ── Heartbeat loop on the control stream ────────────────────────────────

func heartbeatLoopWT(
	ctx context.Context,
	stream *webtransport.Stream,
	mu *sync.Mutex,
	interval time.Duration,
) {
	ticker := time.NewTicker(interval)
	defer ticker.Stop()

	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			hb := map[string]interface{}{
				"type":         "heartbeat",
				"timestamp_ms": time.Now().UnixMilli(),
			}
			data, _ := json.Marshal(hb)
			mu.Lock()
			err := writeWTLengthPrefixed(stream, data)
			mu.Unlock()
			if err != nil {
				return
			}
		}
	}
}

// ── Data stream loop ────────────────────────────────────────────────────

// runWTDataStream drives a single long-lived WT data stream: reads a
// length-prefixed batch request, dispatches to the user's processor,
// writes a length-prefixed batch response, forever. Matches the
// server-side `wt_data_stream_reader` + `call_batch` wire protocol.
func runWTDataStream(
	ctx context.Context,
	stream *webtransport.Stream,
	codec *Codec,
	signer *Signer,
	batchSigning bool,
	cfg ConfigWT,
) error {
	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		default:
		}

		frame, err := readWTLengthPrefixed(stream)
		if err != nil {
			return err
		}

		batchID, events, err := DecodeBatchRequest(frame, codec)
		if err != nil {
			return fmt.Errorf("decode batch: %w", err)
		}

		var outputsPerEvent [][]Output
		switch {
		case cfg.BatchProcessor != nil:
			outputsPerEvent = cfg.BatchProcessor(events)
		case cfg.Processor != nil:
			outputsPerEvent = make([][]Output, len(events))
			for i, e := range events {
				outputsPerEvent[i] = cfg.Processor(e)
			}
		default:
			return fmt.Errorf("no processor configured")
		}

		var signature [64]byte
		if batchSigning {
			body := buildResponseBody(batchID, outputsPerEvent, codec)
			signature = signer.SignBatch(body)
		}

		response, err := EncodeBatchResponse(batchID, outputsPerEvent, signature, codec)
		if err != nil {
			return fmt.Errorf("encode response: %w", err)
		}

		if err := writeWTLengthPrefixed(stream, response); err != nil {
			return fmt.Errorf("write response: %w", err)
		}
	}
}

// ── Wire helpers ─────────────────────────────────────────────────────────

// writeWTRoutingHeader writes the once-per-stream routing header that
// identifies which (pipeline, partition) this data stream serves.
// Format (matching `webtransport_host.rs`): [4B name_len LE][name][2B part LE].
func writeWTRoutingHeader(stream io.Writer, pipelineName string, partition uint16) error {
	name := []byte(pipelineName)
	header := make([]byte, 0, 4+len(name)+2)
	var lenBuf [4]byte
	binary.LittleEndian.PutUint32(lenBuf[:], uint32(len(name)))
	header = append(header, lenBuf[:]...)
	header = append(header, name...)
	var partBuf [2]byte
	binary.LittleEndian.PutUint16(partBuf[:], partition)
	header = append(header, partBuf[:]...)
	_, err := stream.Write(header)
	return err
}

// writeWTLengthPrefixed writes [4B len LE][data] on a QUIC stream.
// Used for both control-stream JSON messages and data-stream batch frames.
func writeWTLengthPrefixed(stream io.Writer, data []byte) error {
	var lenBuf [4]byte
	binary.LittleEndian.PutUint32(lenBuf[:], uint32(len(data)))
	if _, err := stream.Write(lenBuf[:]); err != nil {
		return err
	}
	_, err := stream.Write(data)
	return err
}

// readWTLengthPrefixed reads [4B len LE][data] from a QUIC stream and
// returns the data bytes. Enforces a 16 MiB ceiling to guard against a
// hostile or corrupt peer.
func readWTLengthPrefixed(stream io.Reader) ([]byte, error) {
	var lenBuf [4]byte
	if _, err := io.ReadFull(stream, lenBuf[:]); err != nil {
		return nil, err
	}
	msgLen := binary.LittleEndian.Uint32(lenBuf[:])
	if msgLen > 16*1024*1024 {
		return nil, fmt.Errorf("wt frame too large: %d bytes", msgLen)
	}
	buf := make([]byte, msgLen)
	if _, err := io.ReadFull(stream, buf); err != nil {
		return nil, err
	}
	return buf, nil
}
