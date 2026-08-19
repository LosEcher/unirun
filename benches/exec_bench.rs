//! Library-level execution benchmarks (criterion). Run with:
//!
//!   cargo bench
//!
//! Measures the normalized-execution pipeline end-to-end through `exec::run`:
//! spawn + process-group setup, streaming capture, encoding, classification.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use unirun::exec::run;
use unirun::spec::{ExecSpec, Shell};

fn sh(command: &str) -> ExecSpec {
    ExecSpec {
        command: command.to_string(),
        shell: Some(Shell::Bash),
        timeout_ms: 15_000,
        ..Default::default()
    }
}

fn bench_exec(c: &mut Criterion) {
    let mut group = c.benchmark_group("exec");

    group.bench_function("true_through_bash", |b| {
        b.iter(|| run(black_box(&sh("true"))))
    });

    group.bench_function("echo_hello", |b| {
        b.iter(|| run(black_box(&sh("echo hello"))))
    });

    group.bench_function("unicode_output", |b| {
        b.iter(|| run(black_box(&sh("echo '中文输出'"))))
    });

    // 10 MiB of output through the capped, drained capture path.
    group.bench_function("large_output_10mb", |b| {
        b.iter(|| run(black_box(&sh("yes x | head -c 10485760"))))
    });

    // Deadline + whole-tree kill latency (sleep 30 killed after 100 ms).
    group.bench_function("timeout_kill_latency_100ms", |b| {
        b.iter(|| {
            run(black_box(&ExecSpec {
                command: "sleep 30".into(),
                shell: Some(Shell::Bash),
                timeout_ms: 100,
                ..Default::default()
            }))
        })
    });

    group.finish();
}

criterion_group!(benches, bench_exec);
criterion_main!(benches);
