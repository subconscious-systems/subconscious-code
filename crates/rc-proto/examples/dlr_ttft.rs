//! Cross-host request-to-first-SSE benchmark for JSON versus DLR.
//!
//! This intentionally uses an immediate-response gateway so the result isolates
//! client encoding, upload, sidecar reconstruction, and the first response byte.
//! It does not include model queueing or prompt prefill.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rc_proto::{
    AgentStreamEvent, ChatClient, CompleteOpts, DlrMode, RequestPayloadStats, UserContent,
    WireMessage,
};
use tokio_stream::StreamExt;

type DynError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
async fn main() -> Result<(), DynError> {
    let json_url = required("SC_TTFT_JSON_URL")?;
    let dlr_url = required("SC_TTFT_DLR_URL")?;
    let token = std::env::var("SC_TTFT_DLR_TOKEN").ok();
    let sizes = std::env::var("SC_TTFT_SIZES_MIB")
        .unwrap_or_else(|_| "1,10,25,45".into())
        .split(',')
        .map(|value| value.trim().parse::<usize>())
        .collect::<Result<Vec<_>, _>>()?;
    let repeats = std::env::var("SC_TTFT_REPEATS")
        .unwrap_or_else(|_| "3".into())
        .parse::<usize>()?;
    let history_block_bytes = std::env::var("SC_TTFT_HISTORY_BLOCK_KIB")
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()?
        .map(|kib| kib.saturating_mul(1024))
        .filter(|bytes| *bytes > 0);
    let (corpus, corpus_name) = if std::env::var("SC_TTFT_SYNTHETIC_SOURCE").as_deref() == Ok("1") {
        (Some(synthetic_rust_corpus()), "generated-rust")
    } else {
        let corpus = std::env::var_os("SC_TTFT_CORPUS_DIR")
            .map(load_rust_corpus)
            .transpose()?;
        let name = if corpus.is_some() {
            "local-rust-source"
        } else {
            "deterministic-high-entropy"
        };
        (corpus, name)
    };
    eprintln!("corpus={corpus_name}");

    println!("size_mib,repeat,transport,ttft_ms,json_bytes,wire_bytes,reduction_x");
    for size_mib in sizes {
        let size = size_mib * 1024 * 1024;
        let prefix = match &corpus {
            Some(corpus) => repeat_to_size(corpus, size),
            None => source_like_payload(size),
        };
        let mut messages = history_messages(prefix, history_block_bytes);
        eprintln!("size_mib={size_mib} initial_messages={}", messages.len());
        let session = format!("ttft-{size_mib}-{}", std::process::id());

        let dlr = ChatClient::new(
            json_url.clone(),
            "benchmark-key".into(),
            "benchmark".into(),
            None,
        )?
        .with_dlr(dlr_url.clone(), token.clone(), DlrMode::Required, 5)?;
        let raw = ChatClient::new(
            json_url.clone(),
            "benchmark-key".into(),
            "benchmark".into(),
            None,
        )?;
        let gzip = ChatClient::new(
            json_url.clone(),
            "benchmark-key".into(),
            "benchmark".into(),
            None,
        )?
        .with_request_gzip(true);

        let (cold, cold_stats) = measure(&dlr, &messages, &session).await?;
        report(size_mib, 0, "dlr_cold", cold, cold_stats);

        for repeat in 1..=repeats {
            messages.push(WireMessage::Assistant {
                content: Some(Arc::from("ok")),
                reasoning_content: None,
                tool_calls: vec![],
            });
            messages.push(WireMessage::User {
                content: format!("incremental question {repeat}").into(),
            });

            let (raw_ttft, raw_stats) = measure(&raw, &messages, &session).await?;
            report(size_mib, repeat, "json", raw_ttft, raw_stats);
            let (gzip_ttft, gzip_stats) = measure(&gzip, &messages, &session).await?;
            report(size_mib, repeat, "json_gzip", gzip_ttft, gzip_stats);
            let (dlr_ttft, dlr_stats) = measure(&dlr, &messages, &session).await?;
            report(size_mib, repeat, "dlr_steady", dlr_ttft, dlr_stats);
        }
    }
    Ok(())
}

async fn measure(
    client: &ChatClient,
    messages: &[WireMessage],
    session: &str,
) -> Result<(Duration, RequestPayloadStats), DynError> {
    let opts = CompleteOpts {
        session_id: Some(session.to_string()),
        ..Default::default()
    };
    let started = Instant::now();
    let (mut stream, _, stats) = client
        .stream(messages, &opts, &[])
        .await
        .map_err(|(error, _, _)| error)?;
    let mut first = None;
    while let Some(event) = stream.next().await {
        match event? {
            AgentStreamEvent::TransportActivity | AgentStreamEvent::ToolCallProgress { .. } => {}
            _ => {
                first.get_or_insert_with(|| started.elapsed());
            }
        }
    }
    Ok((first.ok_or("gateway produced no SSE model event")?, stats))
}

fn report(
    size_mib: usize,
    repeat: usize,
    transport: &str,
    ttft: Duration,
    stats: RequestPayloadStats,
) {
    let reduction = stats.json_bytes as f64 / stats.wire_bytes.max(1) as f64;
    println!(
        "{size_mib},{repeat},{transport},{:.3},{},{},{reduction:.2}",
        ttft.as_secs_f64() * 1000.0,
        stats.json_bytes,
        stats.wire_bytes,
    );
}

fn required(name: &str) -> Result<String, DynError> {
    std::env::var(name).map_err(|_| format!("{name} must be set").into())
}

/// Deterministic high-entropy ASCII keeps gzip honest and avoids benchmarking
/// a trivial run of one repeated byte. Real source trees are usually more
/// compressible, so gzip production results may land between this and DLR.
fn source_like_payload(size: usize) -> String {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let alphabet = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 _";
    let mut bytes = Vec::with_capacity(size);
    while bytes.len() < size {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        bytes.push(alphabet[(state as usize) % alphabet.len()]);
    }
    // Every selected byte is ASCII.
    String::from_utf8(bytes).expect("ASCII payload")
}

fn load_rust_corpus(root: std::ffi::OsString) -> Result<String, DynError> {
    fn visit(path: &std::path::Path, files: &mut Vec<std::path::PathBuf>) -> std::io::Result<()> {
        if path
            .file_name()
            .is_some_and(|name| matches!(name.to_str(), Some("target" | ".git")))
        {
            return Ok(());
        }
        if path.is_dir() {
            for entry in std::fs::read_dir(path)? {
                visit(&entry?.path(), files)?;
            }
        } else if path.extension().and_then(|value| value.to_str()) == Some("rs") {
            files.push(path.to_path_buf());
        }
        Ok(())
    }

    let mut files = Vec::new();
    visit(std::path::Path::new(&root), &mut files)?;
    files.sort();
    let mut corpus = String::new();
    for path in files {
        if let Ok(source) = std::fs::read_to_string(&path) {
            corpus.push_str("\n// file: ");
            corpus.push_str(&path.to_string_lossy());
            corpus.push('\n');
            corpus.push_str(&source);
        }
    }
    if corpus.is_empty() {
        return Err("corpus directory contained no UTF-8 Rust source".into());
    }
    Ok(corpus)
}

fn repeat_to_size(corpus: &str, size: usize) -> String {
    let mut output = String::with_capacity(size);
    while output.len() < size {
        let remaining = size - output.len();
        if remaining >= corpus.len() {
            output.push_str(corpus);
        } else {
            let mut end = remaining;
            while !corpus.is_char_boundary(end) {
                end -= 1;
            }
            output.push_str(&corpus[..end]);
            while output.len() < size {
                output.push(' ');
            }
        }
    }
    output
}

fn synthetic_rust_corpus() -> String {
    let mut output = String::with_capacity(1024 * 1024);
    for index in 0..12_000 {
        output.push_str(&format!(
            "pub async fn generated_{index}(input: &str) -> Result<String, Error> {{\n    \
             let normalized = input.trim().to_ascii_lowercase();\n    \
             tracing::debug!(operation = {index}, bytes = normalized.len(), \"processing request\");\n    \
             Ok(format!(\"item-{index}-{{normalized}}\"))\n}}\n\n"
        ));
    }
    output
}

fn history_messages(prefix: String, block_bytes: Option<usize>) -> Vec<WireMessage> {
    let Some(block_bytes) = block_bytes else {
        return vec![WireMessage::User {
            content: UserContent::Text(Arc::from(prefix)),
        }];
    };
    let mut messages = Vec::with_capacity(prefix.len().div_ceil(block_bytes));
    let mut start = 0usize;
    while start < prefix.len() {
        let mut end = start.saturating_add(block_bytes).min(prefix.len());
        while !prefix.is_char_boundary(end) {
            end -= 1;
        }
        messages.push(WireMessage::User {
            content: UserContent::Text(Arc::from(&prefix[start..end])),
        });
        start = end;
    }
    messages
}
