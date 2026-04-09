package io.aeon.processor;

import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.MessageDigest;
import java.security.PrivateKey;
import java.security.PublicKey;
import java.security.SecureRandom;
import java.security.Signature;
import java.security.spec.NamedParameterSpec;
import java.util.Base64;
import java.util.HexFormat;

/**
 * ED25519 signer using Java's built-in EdDSA (Java 15+).
 */
public final class Signer {

    private static final String ALGORITHM = "Ed25519";
    private static final HexFormat HEX = HexFormat.of();

    private final PrivateKey privateKey;
    private final PublicKey publicKey;

    private Signer(PrivateKey privateKey, PublicKey publicKey) {
        this.privateKey = privateKey;
        this.publicKey = publicKey;
    }

    /**
     * Generate a new random ED25519 key pair.
     */
    public static Signer generate() {
        try {
            var kpg = KeyPairGenerator.getInstance(ALGORITHM);
            KeyPair kp = kpg.generateKeyPair();
            return new Signer(kp.getPrivate(), kp.getPublic());
        } catch (Exception e) {
            throw new RuntimeException("Failed to generate Ed25519 key pair", e);
        }
    }

    /**
     * Create a signer from a 32-byte seed.
     * Uses a deterministic SecureRandom to feed the seed into KeyPairGenerator,
     * which derives both the private and public key from the seed material.
     */
    public static Signer fromSeed(byte[] seed) {
        if (seed.length != 32) {
            throw new IllegalArgumentException("Ed25519 seed must be exactly 32 bytes, got " + seed.length);
        }
        try {
            var kpg = KeyPairGenerator.getInstance(ALGORITHM);
            kpg.initialize(NamedParameterSpec.ED25519, new DeterministicRandom(seed));
            KeyPair kp = kpg.generateKeyPair();
            return new Signer(kp.getPrivate(), kp.getPublic());
        } catch (Exception e) {
            throw new RuntimeException("Failed to create Ed25519 signer from seed", e);
        }
    }

    /**
     * A SecureRandom that deterministically returns the provided seed bytes.
     * Used to make KeyPairGenerator produce a deterministic key pair from a seed.
     */
    private static final class DeterministicRandom extends SecureRandom {
        private final byte[] fixedBytes;
        private int pos = 0;

        DeterministicRandom(byte[] seed) {
            this.fixedBytes = seed.clone();
        }

        @Override
        public void nextBytes(byte[] bytes) {
            for (int i = 0; i < bytes.length; i++) {
                bytes[i] = fixedBytes[(pos + i) % fixedBytes.length];
            }
            pos += bytes.length;
        }
    }

    /**
     * Raw 32-byte public key as hex string.
     */
    public String publicKeyHex() {
        byte[] raw = rawPublicKey();
        return HEX.formatHex(raw);
    }

    /**
     * Formatted public key: "ed25519:<base64>".
     */
    public String publicKeyFormatted() {
        byte[] raw = rawPublicKey();
        return "ed25519:" + Base64.getEncoder().encodeToString(raw);
    }

    /**
     * SHA-256 fingerprint of the raw public key, as hex.
     */
    public String fingerprint() {
        try {
            var md = MessageDigest.getInstance("SHA-256");
            byte[] hash = md.digest(rawPublicKey());
            return HEX.formatHex(hash);
        } catch (Exception e) {
            throw new RuntimeException("SHA-256 not available", e);
        }
    }

    /**
     * Sign a hex-encoded challenge nonce, return hex signature.
     */
    public String signChallenge(String nonceHex) {
        byte[] nonce = HEX.parseHex(nonceHex);
        byte[] sig = sign(nonce);
        return HEX.formatHex(sig);
    }

    /**
     * Sign arbitrary data, return 64-byte ED25519 signature.
     */
    public byte[] signBatch(byte[] data) {
        return sign(data);
    }

    /**
     * Verify a signature against data using this signer's public key.
     */
    public boolean verify(byte[] data, byte[] signature) {
        try {
            var verifier = Signature.getInstance(ALGORITHM);
            verifier.initVerify(publicKey);
            verifier.update(data);
            return verifier.verify(signature);
        } catch (Exception e) {
            return false;
        }
    }

    /**
     * Get the raw 32-byte public key (last 32 bytes of X.509 encoding).
     */
    public byte[] rawPublicKey() {
        byte[] encoded = publicKey.getEncoded();
        byte[] raw = new byte[32];
        System.arraycopy(encoded, encoded.length - 32, raw, 0, 32);
        return raw;
    }

    private byte[] sign(byte[] data) {
        try {
            var signer = Signature.getInstance(ALGORITHM);
            signer.initSign(privateKey);
            signer.update(data);
            return signer.sign();
        } catch (Exception e) {
            throw new RuntimeException("Ed25519 signing failed", e);
        }
    }
}
