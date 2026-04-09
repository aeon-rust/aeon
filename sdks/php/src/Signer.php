<?php

declare(strict_types=1);

namespace Aeon\ProcessorSdk;

use ParagonIE\Sodium\Compat as Sodium;

/**
 * ED25519 signer for AWPP challenge-response authentication and batch signing.
 *
 * Uses paragonie/sodium_compat for portable ED25519 operations (works without
 * ext-sodium, though ext-sodium is faster when available).
 */
final class Signer
{
    /** 32-byte ED25519 seed. */
    private readonly string $seed;
    /** 64-byte ED25519 secret key (seed + public key). */
    private readonly string $secretKey;
    /** 32-byte ED25519 public key. */
    private readonly string $publicKey;

    private function __construct(string $seed)
    {
        if (strlen($seed) !== SODIUM_CRYPTO_SIGN_SEEDBYTES) {
            throw new \InvalidArgumentException(
                sprintf('ED25519 seed must be %d bytes, got %d', SODIUM_CRYPTO_SIGN_SEEDBYTES, strlen($seed))
            );
        }
        $this->seed = $seed;
        $keyPair = Sodium::crypto_sign_seed_keypair($seed);
        $this->secretKey = Sodium::crypto_sign_secretkey($keyPair);
        $this->publicKey = Sodium::crypto_sign_publickey($keyPair);
    }

    /** Generate a new random keypair. */
    public static function generate(): self
    {
        return new self(random_bytes(SODIUM_CRYPTO_SIGN_SEEDBYTES));
    }

    /** Create from a 32-byte seed. */
    public static function fromSeed(string $seed): self
    {
        return new self($seed);
    }

    /** Load from a 32-byte seed file. */
    public static function fromFile(string $path): self
    {
        $seed = file_get_contents($path);
        if ($seed === false) {
            throw new \RuntimeException("Cannot read seed file: $path");
        }
        return new self($seed);
    }

    /** Hex-encoded 32-byte public key. */
    public function publicKeyHex(): string
    {
        return bin2hex($this->publicKey);
    }

    /** "ed25519:<base64>" format for AWPP registration. */
    public function publicKeyFormatted(): string
    {
        return 'ed25519:' . base64_encode($this->publicKey);
    }

    /** SHA-256 fingerprint of the public key. */
    public function fingerprint(): string
    {
        return hash('sha256', $this->publicKey);
    }

    /**
     * Sign a hex-encoded nonce for AWPP challenge-response.
     *
     * @param string $nonceHex Hex-encoded nonce bytes from the server challenge.
     * @return string Hex-encoded 64-byte ED25519 signature.
     */
    public function signChallenge(string $nonceHex): string
    {
        $nonce = hex2bin($nonceHex);
        if ($nonce === false) {
            throw new \InvalidArgumentException('Invalid hex nonce');
        }
        $sig = Sodium::crypto_sign_detached($nonce, $this->secretKey);
        return bin2hex($sig);
    }

    /**
     * Sign batch response data.
     *
     * @return string 64-byte ED25519 signature (raw bytes).
     */
    public function signBatch(string $data): string
    {
        return Sodium::crypto_sign_detached($data, $this->secretKey);
    }

    /**
     * Verify a signature.
     */
    public function verify(string $data, string $signature): bool
    {
        return Sodium::crypto_sign_verify_detached($signature, $data, $this->publicKey);
    }
}
