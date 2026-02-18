use criterion::{criterion_group, criterion_main, Criterion};

fn placeholder(_c: &mut Criterion) {
    // Benchmarks will be added after sharded wrapper is complete
}

criterion_group!(benches, placeholder);
criterion_main!(benches);
