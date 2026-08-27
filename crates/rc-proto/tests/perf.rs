//! Manual release-mode microbenchmarks for byte-oriented protocol hot paths.
//!
//! Run with:
//! `cargo test -p rc-proto --release --test perf -- --ignored --nocapture`

use rc_proto::SseDecoder;
use std::hint::black_box;
use std::io::Write;
use std::time::Instant;

#[test]
#[ignore]
fn bench_sse_many_lines_in_one_chunk() {
    const LINES: usize = 4_096;
    const ITERS: usize = 20;
    let line = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"}}]}\n";
    let mut input = Vec::with_capacity(line.len() * LINES);
    for _ in 0..LINES {
        input.extend_from_slice(line);
    }

    let start = Instant::now();
    let mut events = 0usize;
    for _ in 0..ITERS {
        let mut decoder = SseDecoder::new();
        events += black_box(decoder.feed(black_box(&input))).len();
    }
    let elapsed = start.elapsed();
    black_box(events);
    eprintln!(
        "SSE {} KiB chunk: {:.2} ms/iteration",
        input.len() / 1024,
        elapsed.as_secs_f64() * 1_000.0 / ITERS as f64,
    );
}

#[test]
#[ignore]
fn bench_gzip_source_payload() {
    const ITERS: usize = 20;
    let source = "fn representative_function() { println!(\"hello world\"); }\n";
    let input = source.repeat((8 * 1024 * 1024) / source.len());

    for (name, level) in [
        ("fast", flate2::Compression::fast()),
        ("default", flate2::Compression::default()),
    ] {
        let start = Instant::now();
        let mut compressed_len = 0usize;
        for _ in 0..ITERS {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::with_capacity(input.len() / 4), level);
            encoder.write_all(black_box(input.as_bytes())).unwrap();
            compressed_len = encoder.finish().unwrap().len();
        }
        let elapsed = start.elapsed();
        eprintln!(
            "gzip {name}: {:.2} ms/iteration, {} -> {} bytes",
            elapsed.as_secs_f64() * 1_000.0 / ITERS as f64,
            input.len(),
            compressed_len,
        );
        black_box(compressed_len);
    }
}
