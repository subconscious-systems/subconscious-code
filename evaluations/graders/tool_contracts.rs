//! Independent development-suite acceptance tests, installed after the agent
//! exits. Existing agent-authored tests are not used as grading evidence.
use rc_core::state::ReadRegistry;
use rc_core::{ChangeJournal, ShellState, Tool, ToolCtx, ToolOutcome};
use rc_tools::{Glob, Grep, GrepMany, Read, ReadMany};
use serde_json::json;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

fn ctx(root: &Path) -> ToolCtx {
    ToolCtx {
        cwd: root.to_path_buf(),
        allowed_roots: vec![root.to_path_buf()],
        cancel: CancellationToken::new(),
        read_registry: Arc::new(Mutex::new(ReadRegistry::new())),
        shell_state: Arc::new(Mutex::new(ShellState::new(root.to_path_buf()))),
        change_journal: Arc::new(Mutex::new(ChangeJournal::new())),
        sandbox: None,
    }
}

fn ok(result: ToolOutcome) -> (String, bool) {
    match result {
        ToolOutcome::Ok {
            content, truncated, ..
        } => (content, truncated),
        other => panic!("expected successful tool output: {other:?}"),
    }
}

mod preserve {
    use super::*;
    #[tokio::test]
    async fn unicode_reads_still_work() {
        let root = tempdir().unwrap();
        std::fs::write(root.path().join("note.txt"), "héllo\n世界\n").unwrap();
        let (text, _) = ok(Read::new()
            .call(json!({"file_path":"note.txt"}), &ctx(root.path()))
            .await
            .unwrap());
        assert!(text.contains("1\théllo"));
        assert!(text.contains("2\t世界"));
    }
    #[tokio::test]
    async fn plain_grep_and_recursive_glob_still_work() {
        let root = tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src/nested")).unwrap();
        std::fs::write(root.path().join("src/nested/a.rs"), "needle\n").unwrap();
        let context = ctx(root.path());
        let (text, _) = ok(Grep::new()
            .call(json!({"pattern":"needle"}), &context)
            .await
            .unwrap());
        assert!(text.contains("a.rs"));
        let (text, _) = ok(Glob::new()
            .call(json!({"pattern":"**/*.rs"}), &context)
            .await
            .unwrap());
        assert!(text.contains("a.rs"));
    }
    #[tokio::test]
    async fn file_containment_is_preserved() {
        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let file = outside.path().join("private.txt");
        std::fs::write(&file, "outside").unwrap();
        let result = Read::new()
            .call(json!({"file_path":file}), &ctx(root.path()))
            .await
            .unwrap();
        assert!(matches!(result, ToolOutcome::Error { .. }));
    }
}

mod goal_glob {
    use super::*;
    #[tokio::test]
    async fn acceptance() {
        let root = tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src/nested")).unwrap();
        for name in ["root.rs", "src/main.rs", "src/nested/leaf.rs"] {
            std::fs::write(root.path().join(name), "x").unwrap();
        }
        for (pattern, expected) in [
            ("*.rs", 1),
            ("src/*.rs", 1),
            ("src/????.rs", 1),
            ("src/**/*.rs", 2),
            ("**/*.rs", 3),
        ] {
            let (text, _) = ok(Glob::new()
                .call(json!({"pattern":pattern}), &ctx(root.path()))
                .await
                .unwrap());
            assert_eq!(text.lines().count(), expected, "pattern {pattern}: {text}");
        }
    }
}

mod goal_grep_modes {
    use super::*;
    #[tokio::test]
    async fn acceptance() {
        for newline in ["\n", "\r\n"] {
            let root = tempdir().unwrap();
            std::fs::write(
                root.path().join("input.txt"),
                format!("préface{newline}alpha{newline}beta{newline}target{newline}end{newline}"),
            )
            .unwrap();
            for (pattern, multiline, lines) in [
                ("^target$", false, vec![":4:target"]),
                (r"alpha\r?\nbeta", true, vec![":2:alpha", ":3:beta"]),
                ("alpha.*beta", true, vec![":2:alpha", ":3:beta"]),
            ] {
                for mode in ["files_with_matches", "count", "content"] {
                    let (text, _) = ok(Grep::new()
                        .call(
                            json!({"pattern":pattern,"multiline":multiline,"output_mode":mode}),
                            &ctx(root.path()),
                        )
                        .await
                        .unwrap());
                    assert!(text.contains("input.txt"), "{pattern}, {mode}: {text}");
                    if mode == "content" {
                        for line in &lines {
                            assert!(text.contains(line), "{text}");
                        }
                    }
                    if mode == "count" {
                        assert!(text.ends_with(":1\n"), "{text}");
                    }
                }
            }
        }
    }
}

mod goal_read_many {
    use super::*;
    #[tokio::test]
    async fn acceptance() {
        let root = tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), "sun\n").unwrap();
        std::fs::write(root.path().join("b.txt"), "moon\n").unwrap();
        for cap in [96, 256, 1024] {
            let context = ctx(root.path());
            let (text, truncated) = ok(ReadMany::with_limits(0, 0, cap)
                .call(json!({"file_paths":["a.txt","b.txt","a.txt"]}), &context)
                .await
                .unwrap());
            assert!(text.contains("sun") && text.contains("moon"), "{text}");
            assert_eq!(text.matches("===== a.txt =====").count(), 1);
            assert!(!truncated);
            assert!(text.len() <= cap);
            for name in ["a.txt", "b.txt"] {
                assert!(context
                    .read_registry
                    .lock()
                    .unwrap()
                    .has_read(&std::fs::canonicalize(root.path().join(name)).unwrap()));
            }
        }
        std::fs::write(root.path().join("a.txt"), "é".repeat(2000)).unwrap();
        std::fs::write(root.path().join("b.txt"), "🦀".repeat(2000)).unwrap();
        let (text, truncated) = ok(ReadMany::with_limits(0, 0, 512)
            .call(json!({"file_paths":["a.txt","b.txt"]}), &ctx(root.path()))
            .await
            .unwrap());
        assert!(truncated && text.len() <= 512);
        assert!(text.contains('é') && text.contains('🦀'));
    }
}

mod goal_grep_many {
    use super::*;
    #[tokio::test]
    async fn acceptance() {
        let root = tempdir().unwrap();
        std::fs::write(root.path().join("data.txt"), "alpha\nbeta\n".repeat(200)).unwrap();
        let (text, truncated) = ok(GrepMany::with_cap(512).call(json!({"queries":[{"pattern":"alpha","output_mode":"content"},{"pattern":"beta","output_mode":"content"}]}), &ctx(root.path())).await.unwrap());
        assert!(truncated && text.len() <= 512);
        assert!(
            text.contains("query 1: alpha") && text.contains("query 2: beta"),
            "{text}"
        );
        for cap in [1, 2, 24, 128] {
            let (text, _) = ok(GrepMany::with_cap(cap)
                .call(
                    json!({"queries":[{"pattern":"alpha","output_mode":"content"}]}),
                    &ctx(root.path()),
                )
                .await
                .unwrap());
            assert!(text.len() <= cap);
        }
    }
}

mod goal_file_glob {
    use super::*;
    #[tokio::test]
    async fn acceptance() {
        let root = tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "needle\n").unwrap();
        for file in [
            "a.rs".to_string(),
            root.path().join("a.rs").to_string_lossy().into_owned(),
        ] {
            for glob in ["*.rs", "a.rs", "*.txt"] {
                for mode in ["files_with_matches", "count", "content"] {
                    let (text, _) = ok(Grep::new()
                        .call(
                            json!({"pattern":"needle","path":file,"glob":glob,"output_mode":mode}),
                            &ctx(root.path()),
                        )
                        .await
                        .unwrap());
                    assert_eq!(
                        text.contains("a.rs"),
                        glob != "*.txt",
                        "{file}, {glob}, {mode}: {text}"
                    );
                }
            }
        }
    }
}
