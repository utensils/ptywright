//! Classifier regression matrix for the built-in claude-code Lua plugin.
//!
//! Auto-enrols every `<name>.txt` fixture under `plugins/claude-code/fixtures/`
//! that has a sibling `<name>.expected.json`. Adding a new fixture is a
//! documentation-only change: drop the two files in and the matrix picks
//! them up.
//!
//! Drives the classifier directly through [`LuaExtension`] + the generic
//! [`ExtensionStateSnapshot`] string surface — there is no typed Rust
//! adapter for Claude Code. Application-specific state vocabulary lives in
//! `plugins/claude-code/main.lua`.

use std::path::PathBuf;

use ptywright::extension::{
    ClassifyContext, Extension, ExtensionStateSnapshot, LuaExtension, STATUS_BAR_ROWS,
    split_status_bar,
};

/// Mirror the host-side stability threshold the RPC server uses for
/// `adapter.*` so the classifier sees the same `completed_turn_stable_ms`
/// it would see in production.
const COMPLETED_TURN_STABLE_MS: u64 = 300;

#[derive(serde::Deserialize)]
struct Expectation {
    state: String,
    evidence: String,
    #[serde(default)]
    last_intent: Option<String>,
    #[serde(default = "default_min_confidence")]
    min_confidence: f32,
    /// Optional structured plugin metadata that the classifier should attach
    /// to `ExtensionStateSnapshot::metadata`. When present, the matrix asserts
    /// deep equality against the parsed classifier output. Fixtures that
    /// don't need a metadata check leave the field absent.
    #[serde(default)]
    metadata: Option<serde_json::Value>,
}

fn default_min_confidence() -> f32 {
    0.6
}

fn classify_fixture(
    extension: &LuaExtension,
    screen: &str,
    sequence: u64,
    last_intent: Option<&str>,
) -> ptywright::Result<ExtensionStateSnapshot> {
    let (body_text, status_text) = split_status_bar(screen, STATUS_BAR_ROWS);
    let markers = std::collections::BTreeMap::new();
    let ctx = ClassifyContext {
        screen,
        body_text: &body_text,
        status_text: &status_text,
        transcript: "",
        sequence,
        last_intent,
        stable_ms: Some(COMPLETED_TURN_STABLE_MS),
        completed_turn_stable_ms: Some(COMPLETED_TURN_STABLE_MS),
        markers: &markers,
        cursor: 0,
        last_event_seq: None,
    };
    extension.classify(&ctx)
}

#[test]
fn classifier_matches_sanitized_claude_code_fixtures() {
    let fixtures_dir: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("plugins")
        .join("claude-code")
        .join("fixtures");

    let mut entries: Vec<PathBuf> = std::fs::read_dir(&fixtures_dir)
        .unwrap_or_else(|err| panic!("read fixtures dir {}: {err}", fixtures_dir.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().and_then(|s| s.to_str()) == Some("txt"))
        .collect();
    entries.sort();

    assert!(
        !entries.is_empty(),
        "no .txt fixtures found in {}",
        fixtures_dir.display()
    );

    let extension =
        LuaExtension::built_in("claude-code").expect("load built-in claude-code Lua plugin");
    let mut asserted = 0usize;

    for (index, txt_path) in entries.into_iter().enumerate() {
        let fixture_name = txt_path
            .file_name()
            .and_then(|s| s.to_str())
            .map(ToString::to_string)
            .unwrap_or_else(|| txt_path.display().to_string());

        let expected_path = txt_path.with_extension("expected.json");
        if !expected_path.exists() {
            println!(
                "skipping fixture {fixture_name}: missing sibling {}",
                expected_path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("<expected>.json")
            );
            continue;
        }

        let fixture_body = std::fs::read_to_string(&txt_path)
            .unwrap_or_else(|err| panic!("read fixture {fixture_name}: {err}"));
        let expected_raw = std::fs::read_to_string(&expected_path)
            .unwrap_or_else(|err| panic!("read expectations for {fixture_name}: {err}"));
        let expectation: Expectation = serde_json::from_str(&expected_raw)
            .unwrap_or_else(|err| panic!("parse expectations for {fixture_name}: {err}"));

        let state = classify_fixture(
            &extension,
            &fixture_body,
            index as u64,
            expectation.last_intent.as_deref(),
        )
        .unwrap_or_else(|err| {
            panic!(
                "classify fixture {fixture_name} via Lua plugin: {err}\n--- fixture body ---\n{fixture_body}"
            )
        });

        assert_eq!(
            state.state, expectation.state,
            "fixture {fixture_name} classified as {:?} with evidence: {}",
            state.state, state.evidence
        );
        assert_eq!(
            state.evidence, expectation.evidence,
            "fixture {fixture_name} evidence mismatch"
        );
        assert!(
            state.confidence >= expectation.min_confidence,
            "fixture {fixture_name} confidence {} below floor {}",
            state.confidence,
            expectation.min_confidence
        );
        if let Some(expected_metadata) = expectation.metadata.as_ref() {
            let actual = state.metadata.as_ref().unwrap_or_else(|| {
                panic!(
                    "fixture {fixture_name} expected metadata {expected_metadata} but classifier returned none"
                )
            });
            assert_eq!(
                actual, expected_metadata,
                "fixture {fixture_name} metadata mismatch"
            );
        }
        asserted += 1;
    }

    assert!(
        asserted > 0,
        "no fixtures had sibling .expected.json files under {}",
        fixtures_dir.display()
    );
}
