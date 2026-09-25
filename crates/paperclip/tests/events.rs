use paper_core::{store, ClipStatus};
use serde_json::{json, Value};
use std::path::Path;
use tempfile::TempDir;

/// Run the real CLI against isolated logs with deterministic attribution and time.
/// @param file Clip log path.
/// @param args Command arguments.
/// @return Parsed successful response.
fn run(file: &Path, args: &[&str]) -> Value {
    let output = assert_cmd::cargo::cargo_bin_cmd!("paperclip")
        .env("PAPERCUTS_AGENT", "environment")
        .env("PAPERCUTS_NOW", "2026-09-24T00:00:00Z")
        .env("PAPERCUTS_FILE", file.with_file_name("cuts.jsonl"))
        .env("PAPERCLIP_FILE", file)
        .args(args).output().unwrap();
    assert!(output.status.success(), "{:?}", output);
    serde_json::from_slice(&output.stdout).unwrap()
}

/// Prove writers survive reading, agent flag precedence, and promoted idempotence.
#[test]
fn written_events_round_trip() {
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("clips.jsonl");
    std::fs::write(temp.path().join("cuts.jsonl"), "").unwrap();
    let added = run(&file, &["add", "Useful observation"]);
    let id = added["data"]["record"]["id"].as_str().unwrap();
    run(&file, &["note", id, "Retained note", "--agent", "flag"]);
    let listed = run(&file, &["list", "--status", "all"]);
    assert_eq!(listed["data"]["items"][0]["status"], "noted");
    assert_eq!(listed["data"]["items"][0]["notes"][0]["text"], "Retained note");
    assert_eq!(listed["data"]["items"][0]["notes"][0]["agent"], "flag");
    run(&file, &["promote", id]);
    assert_eq!(run(&file, &["list", "--status", "all"])["data"]["items"][0]["status"], "promoted");
    let events = std::fs::read_to_string(&file).unwrap();
    let promoted: Value = serde_json::from_str(events.lines().last().unwrap()).unwrap();
    assert_eq!(promoted["agent"], "environment");
    run(&file, &["promote", id, "--agent", "second"]);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), events);
    assert!(run(&file, &["review"])["meta"]["warnings"].is_null());
}

/// Supply a valid clip to distinguish malformed events from orphan events.
/// @return A complete clip record.
fn clip() -> Value {
    json!({"kind":"clip", "id":"cl_001122334455", "ts":"2026-09-24T00:00:00Z",
        "agent":"author", "text":"win", "tags":[], "impact":"solid", "cwd":".", "repo":null})
}

/// Accept only historical note/promote omissions, without inventing an agent.
#[test]
fn legacy_events_keep_notes_and_promotions() {
    let note = json!({"kind":"note", "id":"cl_001122334455", "ts":"2026-09-24T00:00:00Z", "text":"legacy"});
    let promote = json!({"kind":"promote", "id":"cl_001122334455", "ts":"2026-09-24T00:00:00Z"});
    let folded = store::fold_clip_bytes(format!("{}\n{}\n", clip(), note).as_bytes());
    assert_eq!(folded.items[0].status, ClipStatus::Noted);
    assert_eq!(folded.items[0].notes[0].text, "legacy");
    assert_eq!(folded.items[0].notes[0].agent, None);
    assert!(serde_json::to_value(&folded.items[0]).unwrap()["notes"][0].get("agent").is_none());
    assert!(folded.warnings.is_empty());
    let folded = store::fold_clip_bytes(format!("{}\n{}\n{}\n", clip(), note, promote).as_bytes());
    assert_eq!(folded.items[0].status, ClipStatus::Promoted);
    assert!(folded.warnings.is_empty());
}

/// Keep all other required fields and present agent types strict.
#[test]
fn malformed_events_still_warn() {
    for kind in ["note", "promote"] {
        let mut event = json!({"kind":kind, "id":"cl_001122334455", "ts":"2026-09-24T00:00:00Z"});
        if kind == "note" { event["text"] = json!("note"); }
        let mut invalid = Vec::new();
        for field in if kind == "note" { vec!["id", "ts", "text"] } else { vec!["id", "ts"] } {
            let mut missing = event.clone();
            missing.as_object_mut().unwrap().remove(field);
            invalid.push(missing);
        }
        let mut bad_ts = event.clone();
        bad_ts["ts"] = json!("not RFC3339");
        invalid.push(bad_ts);
        for agent in [Value::Null, json!(5)] {
            let mut bad_agent = event.clone();
            bad_agent["agent"] = agent;
            invalid.push(bad_agent);
        }
        for invalid in invalid {
            let folded = store::fold_clip_bytes(format!("{}\n{}\n", clip(), invalid).as_bytes());
            assert_eq!(folded.items[0].status, ClipStatus::Open, "{invalid}");
            assert_eq!(folded.warnings, ["skipped 1 malformed line"], "{invalid}");
        }
    }
    let mut missing_agent = clip();
    missing_agent.as_object_mut().unwrap().remove("agent");
    let folded = store::fold_clip_bytes(format!("{}\n", missing_agent).as_bytes());
    assert!(folded.items.is_empty());
    assert_eq!(folded.warnings, ["skipped 1 malformed line"]);
    for event in [
        json!({"kind":"cut", "id":"pc_001122334455", "ts":"2026-09-24T00:00:00Z", "text":"cut", "tags":[], "severity":"minor", "cwd":".", "repo":null}),
        json!({"kind":"resolve", "id":"pc_001122334455", "ts":"2026-09-24T00:00:00Z", "note":null}),
    ] {
        let folded = store::fold_bytes(format!("{}\n", event).as_bytes());
        assert!(folded.items.is_empty());
        assert_eq!(folded.warnings, ["skipped 1 malformed line"]);
    }
}
