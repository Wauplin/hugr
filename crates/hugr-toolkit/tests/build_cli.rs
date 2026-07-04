//! T2.1 — `hugr build --surface cli`.
//!
//! The fast tests exercise the *runtime* of a built binary in-process: pack a
//! scaffolded definition into a bundle, unpack it into a temp home (as the
//! binary does on startup), assemble the agent, and drive an ask + resume + the
//! `--describe` audit view — no `cargo build`, no network. The heavy
//! end-to-end test (`real_build_produces_a_runnable_binary`) actually invokes
//! `hugr_toolkit::build::build_cli` and runs the produced binary; it is
//! `#[ignore]`d because compiling a detached shim crate is slow.

use std::path::{Path, PathBuf};

use hugr_agent::{AnswerStatus, Ask, TraceId};
use hugr_toolkit::bundle;
use hugr_toolkit::manifest::AgentDefinition;
use hugr_toolkit::runtime::build_agent;
use hugr_toolkit::scaffold::{Template, scaffold_files};

/// Write a scaffolded definition to a temp dir and pack it into a bundle.
fn scaffold_bundle(name: &str, template: Template) -> (Vec<u8>, PathBuf) {
    let src = std::env::temp_dir().join(format!("hugr-bcli-src-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&src);
    for file in scaffold_files(name, template) {
        let path = src.join(&file.rel_path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, file.contents).unwrap();
    }
    let bytes = bundle::pack(&src, &[".hugr-traces", ".scratch"]).unwrap();
    (bytes, src)
}

/// Unpack a bundle into a fresh home dir and load the definition — exactly what
/// a built binary does on startup.
fn unpack_home(bytes: &[u8], tag: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!("hugr-bcli-home-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    bundle::unpack(bytes, &home).unwrap();
    home
}

#[tokio::test]
async fn embedded_definition_answers_and_resumes_from_a_temp_home() {
    // The docs template ships a docs/ folder so fs_read's jail root exists once
    // unpacked — proving the artifact carries its tool data, not just config.
    let (bytes, src) = scaffold_bundle("bcli-docs", Template::Docs);
    let home = unpack_home(&bytes, "docs");
    assert!(
        home.join("docs").is_dir(),
        "tool data unpacked with the bundle"
    );

    let def = AgentDefinition::load(&home).unwrap();
    let (agent, _warnings) = build_agent(&def).await.unwrap();

    // No API key / unreachable endpoint → the model call fails, so this is an
    // *error answer* (exit 0 semantics), but it still persists a root trace.
    let answer = agent.ask(Ask::new("What is the setup?")).await.unwrap();
    assert_eq!(answer.status, AnswerStatus::Error);
    assert!(
        !answer.trace_id.as_str().is_empty(),
        "a trace was persisted"
    );

    // Resume by that trace id → a child trace with depends_on set. Proves the
    // trace store persisted in the home dir is resumable across "invocations".
    let follow = agent
        .ask(Ask::new("And after that?").with_trace_id(answer.trace_id.clone()))
        .await
        .unwrap();
    assert_ne!(
        follow.trace_id, answer.trace_id,
        "resume wrote a new child trace"
    );

    // Both traces are visible from the store the binary would read.
    let heads = agent.traces().unwrap();
    assert!(heads.len() >= 2, "root + child persisted: {}", heads.len());
    let child = heads
        .iter()
        .find(|h| h.trace_id == follow.trace_id)
        .expect("child head present");
    assert_eq!(child.depends_on.as_ref(), Some(&answer.trace_id));

    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn embedded_definition_self_describes() {
    let (bytes, src) = scaffold_bundle("bcli-desc", Template::Blank);
    let home = unpack_home(&bytes, "desc");
    let def = AgentDefinition::load(&home).unwrap();
    let (agent, _) = build_agent(&def).await.unwrap();

    let card = agent.describe();
    assert_eq!(card.name, "bcli-desc");
    // The card serializes (the `--describe` view is JSON).
    let json = serde_json::to_string(&card).unwrap();
    assert!(json.contains("\"scratch_write\""), "{json}");

    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&home);
}

/// End-to-end: compile a real self-contained binary and run its audit view.
/// Ignored by default — building a detached shim crate recompiles the whole
/// dependency tree and is slow. Run with `cargo test -p hugr-toolkit --
/// --ignored real_build`.
#[test]
#[ignore = "invokes cargo build; slow"]
fn real_build_produces_a_runnable_binary() {
    use hugr_toolkit::build::{BuildOptions, build_cli};

    let (_, src) = scaffold_bundle("bcli-e2e", Template::Blank);
    let def = AgentDefinition::load(&src).unwrap();
    let out = std::env::temp_dir().join(format!("hugr-bcli-out-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);

    let outcome = build_cli(
        &def,
        &BuildOptions {
            out_dir: out.clone(),
            release: false,
        },
    )
    .expect("build succeeds");
    assert!(
        outcome.binary.exists(),
        "binary at {}",
        outcome.binary.display()
    );

    // Run `--describe` from a dir with no repo checkout in scope; point the
    // agent home at a throwaway dir so we don't touch the real data dir.
    let home = out.join("home");
    let output = run_binary(&outcome.binary, &["--describe"], &home);
    assert!(output.contains("\"name\": \"bcli-e2e\""), "{output}");

    // And an ask returns a JSON answer at exit 0 (error answer, no model).
    let ask_out = run_binary(&outcome.binary, &["hi there"], &home);
    assert!(ask_out.contains("\"status\": \"error\""), "{ask_out}");

    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&out);
}

fn run_binary(bin: &Path, args: &[&str], home: &Path) -> String {
    let output = std::process::Command::new(bin)
        .args(args)
        .env("HUGR_AGENT_HOME", home)
        .output()
        .expect("run built binary");
    assert!(output.status.success(), "exit 0: {:?}", output.status);
    String::from_utf8_lossy(&output.stdout).into_owned()
}
