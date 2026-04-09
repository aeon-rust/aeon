// Aeon Processor SDK — ED25519 authentication for AWPP handshake and batch signing.

using System.Security.Cryptography;
using NSec.Cryptography;

namespace Aeon.ProcessorSdk;

/// <summary>
/// ED25519 signer for AWPP challenge-response authentication and batch signing.
/// Uses NSec.Cryptography (libsodium) for ED25519 operations.
/// </summary>
public sealed class Signer : IDisposable
{
    private readonly Key _privateKey;
    private readonly PublicKey _publicKey;
    private readonly byte[] _publicKeyBytes;

    private Signer(Key privateKey)
    {
        _privateKey = privateKey;
        _publicKey = privateKey.PublicKey;
        _publicKeyBytes = _publicKey.Export(KeyBlobFormat.RawPublicKey);
    }

    /// <summary>Generate a new random ED25519 keypair.</summary>
    public static Signer Generate()
    {
        var key = Key.Create(SignatureAlgorithm.Ed25519, new KeyCreationParameters
        {
            ExportPolicy = KeyExportPolicies.AllowPlaintextExport,
        });
        return new Signer(key);
    }

    /// <summary>Create a signer from a 32-byte seed.</summary>
    public static Signer FromSeed(ReadOnlySpan<byte> seed)
    {
        if (seed.Length != 32)
            throw new ArgumentException($"ED25519 seed must be 32 bytes, got {seed.Length}");
        var key = Key.Import(SignatureAlgorithm.Ed25519, seed, KeyBlobFormat.RawPrivateKey,
            new KeyCreationParameters { ExportPolicy = KeyExportPolicies.AllowPlaintextExport });
        return new Signer(key);
    }

    /// <summary>Load a signer from a 32-byte seed file.</summary>
    public static Signer FromFile(string path)
    {
        var seed = File.ReadAllBytes(path);
        return FromSeed(seed);
    }

    /// <summary>Hex-encoded 32-byte public key.</summary>
    public string PublicKeyHex => Convert.ToHexString(_publicKeyBytes).ToLowerInvariant();

    /// <summary>"ed25519:base64" format for AWPP registration.</summary>
    public string PublicKeyFormatted => $"ed25519:{Convert.ToBase64String(_publicKeyBytes)}";

    /// <summary>SHA-256 fingerprint of the public key.</summary>
    public string Fingerprint
    {
        get
        {
            var hash = SHA256.HashData(_publicKeyBytes);
            return Convert.ToHexString(hash).ToLowerInvariant();
        }
    }

    /// <summary>
    /// Sign a hex-encoded nonce for AWPP challenge-response.
    /// </summary>
    /// <param name="nonceHex">Hex-encoded nonce bytes from the server challenge.</param>
    /// <returns>Hex-encoded 64-byte ED25519 signature.</returns>
    public string SignChallenge(string nonceHex)
    {
        var nonce = Convert.FromHexString(nonceHex);
        var sig = SignatureAlgorithm.Ed25519.Sign(_privateKey, nonce);
        return Convert.ToHexString(sig).ToLowerInvariant();
    }

    /// <summary>
    /// Sign batch response data.
    /// </summary>
    /// <returns>64-byte ED25519 signature.</returns>
    public byte[] SignBatch(ReadOnlySpan<byte> data)
    {
        return SignatureAlgorithm.Ed25519.Sign(_privateKey, data);
    }

    /// <summary>
    /// Verify a signature against data.
    /// </summary>
    public bool Verify(ReadOnlySpan<byte> data, ReadOnlySpan<byte> signature)
    {
        return SignatureAlgorithm.Ed25519.Verify(_publicKey, data, signature);
    }

    public void Dispose() => _privateKey.Dispose();
}
