//! Benchmarks for aeon-crypto: hashing, Merkle tree, PoH chain, signing, encryption.

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};

use aeon_crypto::encryption::EtmKey;
use aeon_crypto::hash::{Hash512, sha512};
use aeon_crypto::merkle::MerkleTree;
use aeon_crypto::mmr::MerkleMountainRange;
use aeon_crypto::poh::PohChain;
use aeon_crypto::signing::SigningKey;
use aeon_types::PartitionId;

fn bench_sha512(c: &mut Criterion) {
    let mut group = c.benchmark_group("sha512");

    for &size in &[64, 256, 1024, 4096] {
        let data = vec![0xABu8; size];
        group.bench_with_input(BenchmarkId::new("hash", size), &data, |b, data| {
            b.iter(|| sha512(black_box(data)))
        });
    }

    group.finish();
}

fn bench_merkle_tree(c: &mut Criterion) {
    let mut group = c.benchmark_group("merkle_tree");

    for &count in &[10, 100, 1000, 10000] {
        let payloads: Vec<Vec<u8>> = (0..count)
            .map(|i| format!("event-payload-{i:06}").into_bytes())
            .collect();
        let refs: Vec<&[u8]> = payloads.iter().map(|p| p.as_slice()).collect();

        group.bench_with_input(BenchmarkId::new("build", count), &refs, |b, refs| {
            b.iter(|| MerkleTree::from_data(black_box(refs)))
        });
    }

    // Proof generation
    let payloads: Vec<Vec<u8>> = (0..1000)
        .map(|i| format!("event-{i:06}").into_bytes())
        .collect();
    let refs: Vec<&[u8]> = payloads.iter().map(|p| p.as_slice()).collect();
    let tree = MerkleTree::from_data(&refs).unwrap();

    group.bench_function("proof_1000_leaves", |b| {
        b.iter(|| tree.proof(black_box(500)))
    });

    // Proof verification
    let proof = tree.proof(500).unwrap();
    group.bench_function("verify_1000_leaves", |b| {
        b.iter(|| black_box(&proof).verify())
    });

    group.finish();
}

fn bench_mmr(c: &mut Criterion) {
    let mut group = c.benchmark_group("mmr");

    // Append throughput
    let hashes: Vec<Hash512> = (0..10000)
        .map(|i| sha512(format!("leaf-{i}").as_bytes()))
        .collect();

    group.bench_function("append_10000", |b| {
        b.iter(|| {
            let mut mmr = MerkleMountainRange::new();
            for h in &hashes {
                mmr.append(black_box(*h));
            }
            mmr
        })
    });

    // Root computation after many appends
    let mut mmr = MerkleMountainRange::new();
    for h in &hashes {
        mmr.append(*h);
    }
    group.bench_function("root_10000", |b| b.iter(|| mmr.root()));

    // Peaks computation
    group.bench_function("peaks_10000", |b| b.iter(|| mmr.peaks()));

    group.finish();
}

fn bench_poh_chain(c: &mut Criterion) {
    let mut group = c.benchmark_group("poh_chain");

    // Unsigned batch append
    let payloads: Vec<Vec<u8>> = (0..100)
        .map(|i| format!("event-{i:06}").into_bytes())
        .collect();
    let refs: Vec<&[u8]> = payloads.iter().map(|p| p.as_slice()).collect();

    group.bench_function("append_batch_100_unsigned", |b| {
        let mut chain = PohChain::new(PartitionId::new(0), 1000);
        let mut ts = 0i64;
        b.iter(|| {
            ts += 1_000_000;
            chain.append_batch(black_box(&refs), ts, None)
        })
    });

    // Signed batch append
    let key = SigningKey::generate();
    group.bench_function("append_batch_100_signed", |b| {
        let mut chain = PohChain::new(PartitionId::new(0), 1000);
        let mut ts = 0i64;
        b.iter(|| {
            ts += 1_000_000;
            chain.append_batch(black_box(&refs), ts, Some(&key))
        })
    });

    // Single-event batch (minimum overhead)
    group.bench_function("append_batch_1_unsigned", |b| {
        let mut chain = PohChain::new(PartitionId::new(0), 1000);
        let single: Vec<&[u8]> = vec![b"single-event"];
        let mut ts = 0i64;
        b.iter(|| {
            ts += 1_000_000;
            chain.append_batch(black_box(&single), ts, None)
        })
    });

    group.finish();
}

fn bench_signing(c: &mut Criterion) {
    let mut group = c.benchmark_group("ed25519");

    let key = SigningKey::generate();
    let root = sha512(b"merkle-root-to-sign");
    let vk = key.verifying_key();

    group.bench_function("sign_root", |b| {
        b.iter(|| key.sign_root(black_box(&root), 1000))
    });

    let signed = key.sign_root(&root, 1000);
    group.bench_function("verify_root", |b| b.iter(|| black_box(&signed).verify()));

    group.bench_function("verify_with_key", |b| {
        b.iter(|| signed.verify_with_key(black_box(&vk)))
    });

    group.bench_function("keygen", |b| b.iter(SigningKey::generate));

    group.finish();
}

fn bench_etm_encryption(c: &mut Criterion) {
    let mut group = c.benchmark_group("etm_encryption");

    let key = EtmKey::generate();

    for &size in &[64, 256, 1024, 4096, 65536] {
        let plaintext = vec![0xABu8; size];

        group.bench_with_input(BenchmarkId::new("encrypt", size), &plaintext, |b, pt| {
            b.iter(|| key.encrypt(black_box(pt)))
        });

        let ciphertext = key.encrypt(&plaintext).unwrap();
        group.bench_with_input(BenchmarkId::new("decrypt", size), &ciphertext, |b, ct| {
            b.iter(|| key.decrypt(black_box(ct)))
        });
    }

    // Roundtrip (encrypt + decrypt)
    let payload_1k = vec![0xCDu8; 1024];
    group.bench_function("roundtrip_1KB", |b| {
        b.iter(|| {
            let ct = key.encrypt(black_box(&payload_1k)).unwrap();
            key.decrypt(black_box(&ct)).unwrap()
        })
    });

    group.bench_function("keygen", |b| b.iter(EtmKey::generate));

    group.finish();
}

criterion_group!(
    benches,
    bench_sha512,
    bench_merkle_tree,
    bench_mmr,
    bench_poh_chain,
    bench_signing,
    bench_etm_encryption,
);
criterion_main!(benches);
