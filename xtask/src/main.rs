//! xtask: repository build tasks (§1).
//!
//! Currently hosts `bench` — a benchmark runner that orders N tasks via cyclic
//! group (ℤ/n) rotations rather than a fixed order or random shuffle, to
//! reduce order-effect variance. See [`bench`] for the rationale.

mod bench;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("bench") => bench_cmd(&args[1..]),
        Some("bench-orders") => bench_orders_cmd(&args[1..]),
        Some(other) => {
            eprintln!("xtask: unknown subcommand `{other}`");
            eprintln!("usage:");
            eprintln!("  xtask bench [--rotations <k>] <cmd> <cmd> ...   time each shell command under cyclic rotations");
            eprintln!("  xtask bench-orders [--rotations <k>] <n>        print the orderings that `bench` would use for n tasks");
            ExitCode::from(2)
        }
        None => {
            eprintln!("xtask: no subcommand given.");
            eprintln!("  `bench`      time N shell commands under ℤ/n rotation orderings (variance reduction)");
            eprintln!("  `bench-orders <n>`  print the rotation orderings for n tasks without running them");
            ExitCode::from(2)
        }
    }
}

/// Parse an optional `--rotations <k>` from the front of the args, returning
/// `(remaining_args, rotations)` where `rotations == None` means "all n".
fn parse_rotations(args: &[String]) -> (Vec<String>, Option<usize>) {
    let mut rest: Vec<String> = args.to_vec();
    let mut rotations = None;
    if let Some(flag) = rest.first() {
        if flag == "--rotations" {
            if let Some(val) = rest.get(1) {
                if let Ok(k) = val.parse::<usize>() {
                    rotations = Some(k);
                    rest.drain(0..2);
                }
            }
        }
    }
    (rest, rotations)
}

fn bench_cmd(args: &[String]) -> ExitCode {
    let (cmds, rotations) = parse_rotations(args);
    if cmds.is_empty() {
        eprintln!("xtask bench: need at least one command");
        return ExitCode::from(2);
    }
    let tasks: Vec<bench::BenchTask> = cmds
        .iter()
        .map(|c| bench::BenchTask {
            label: shorten(c, 24).to_string(),
            command: c.clone(),
        })
        .collect();
    let orderings = bench::rotations_subset(tasks.len(), rotations.unwrap_or(tasks.len()));
    if orderings.is_empty() {
        eprintln!(
            "xtask bench: no orderings (got {} tasks, {:?} rotations)",
            tasks.len(),
            rotations
        );
        return ExitCode::from(2);
    }
    let stats = bench::run_bench(&tasks, &orderings);
    print!("{}", bench::format_report(&stats));
    ExitCode::SUCCESS
}

fn bench_orders_cmd(args: &[String]) -> ExitCode {
    let (rest, rotations) = parse_rotations(args);
    let n: usize = match rest.first().map(|s| s.parse::<usize>()) {
        Some(Ok(n)) => n,
        _ => {
            eprintln!("xtask bench-orders: need a task count, e.g. `xtask bench-orders 5`");
            return ExitCode::from(2);
        }
    };
    let orderings = bench::rotations_subset(n, rotations.unwrap_or(n));
    for (i, order) in orderings.iter().enumerate() {
        println!("rotation {i}: {order:?}");
    }
    println!(
        "latin-square invariant holds: {}",
        bench::is_latin_square(&orderings)
    );
    ExitCode::SUCCESS
}

fn shorten(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..s
            .char_indices()
            .take(max)
            .last()
            .map(|(i, _)| i)
            .unwrap_or(s.len())]
    }
}
