//! Benchmarks for the posting-list codec and intersection — the query hot path
//! optimized in milestone M3.
//!
//! Run with `cargo bench -p rlocate-core`.

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use rlocate_core::postings;

/// A plausible posting list: `n` docids with small, bursty gaps.
fn make_list(n: usize, seed: u64) -> Vec<u32> {
    let mut v = Vec::with_capacity(n);
    let mut acc = 0u32;
    let mut s = seed;
    for _ in 0..n {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let gap = 1 + ((s >> 40) as u32 % 64);
        acc = acc.wrapping_add(gap);
        v.push(acc);
    }
    v
}

fn bench_codec(c: &mut Criterion) {
    let mut group = c.benchmark_group("postings_codec");
    for &n in &[128usize, 1_024, 100_000] {
        let docids = make_list(n, 0x1234);
        let mut encoded = Vec::new();
        postings::encode(&docids, &mut encoded);

        group.throughput(Throughput::Elements(n as u64));
        group.bench_function(format!("encode/{n}"), |b| {
            b.iter(|| {
                let mut out = Vec::with_capacity(encoded.len());
                postings::encode(black_box(&docids), &mut out);
                black_box(out);
            })
        });
        group.bench_function(format!("decode/{n}"), |b| {
            b.iter(|| black_box(postings::decode(black_box(&encoded), n).unwrap()))
        });
    }
    group.finish();
}

fn bench_intersect(c: &mut Criterion) {
    // One large list, one small — the common "rare trigram prunes" shape.
    let big = make_list(100_000, 1);
    let small = make_list(2_000, 2);
    c.bench_function("intersect/100k_x_2k", |b| {
        b.iter(|| black_box(postings::intersect(vec![big.clone(), small.clone()])))
    });
}

criterion_group!(benches, bench_codec, bench_intersect);
criterion_main!(benches);
