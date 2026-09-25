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

/// Run Markdown listing against an isolated log.
/// @param file Clip log path, which may not exist.
/// @param args Additional list filters and limits.
/// @return Successful stdout as UTF-8.
fn markdown(file: &Path, args: &[&str]) -> String {
    let output = assert_cmd::cargo::cargo_bin_cmd!("paperclip")
        .env("PAPERCLIP_FILE", file)
        .args(["list", "--format", "md"])
        .args(args).output().unwrap();
    assert!(output.status.success(), "{:?}", output);
    assert!(output.stderr.is_empty());
    String::from_utf8(output.stdout).unwrap()
}

/// Render each clip on one line, retaining location, impact, and lifecycle state.
#[test]
fn markdown_lists_clip_details_and_status() {
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("clips.jsonl");
    for (text, impact, location, event) in [
        ("Open\nobservation", "nice", Some("probe/one"), None),
        ("Noted observation", "solid", Some("probe/two"), Some("note")),
        ("Promoted observation", "huge", None, Some("promote")),
    ] {
        let mut args = vec!["add", text, "--impact", impact];
        if let Some(location) = location {
            args.extend(["--where", location]);
        }
        let added = run(&file, &args);
        let id = added["data"]["record"]["id"].as_str().unwrap();
        if let Some(event) = event {
            let args = if event == "note" { vec![event, id, "Retained"] } else { vec![event, id] };
            run(&file, &args);
        }
        let md = markdown(&file, &["--status", "all"]);
        let line = md.lines().find(|line| line.starts_with(&format!("- [{id}]"))).unwrap();
        assert!(line.contains(&text.replace('\n', " ")));
        assert!(line.contains(impact));
        if let Some(location) = location {
            assert!(line.contains(location));
        } else {
            assert!(!line.contains("where:"));
        }
        match event {
            Some("note") => assert!(line.contains("noted")),
            Some("promote") => assert!(line.contains("promoted")),
            _ => assert!(!line.contains(", open")),
        }
    }
    assert_eq!(markdown(&file, &["--status", "all"]).lines().filter(|line| line.starts_with("- [")).count(), 3);
}

/// A missing log remains successful and produces no JSON in Markdown mode.
#[test]
fn missing_clip_log_honors_format() {
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("missing.jsonl");
    assert_eq!(markdown(&file, &[]), "");
    let listed = run(&file, &["list"]);
    assert_eq!(listed["data"], json!({"items":[], "count":0, "total":0, "truncated":false}));
    assert_eq!(listed["meta"], json!({"contract":1}));
    assert!(!file.exists());
}

/// Compute truncation after filtering and share the warning across output formats.
#[test]
fn clip_truncation_is_visible_after_filtering() {
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("clips.jsonl");
    for text in ["first", "second", "third"] {
        run(&file, &["add", text, "--where", "selected"]);
    }
    run(&file, &["add", "outside filter", "--where", "other"]);
    for (limit, count, truncated) in [("0", 0, true), ("2", 2, true), ("3", 3, false)] {
        let args = ["--where", "selected", "--limit", limit];
        let listed = run(&file, &[&["list"], &args[..]].concat());
        assert_eq!(listed["data"]["count"], count);
        assert_eq!(listed["data"]["items"].as_array().unwrap().len(), count);
        assert_eq!(listed["data"]["total"], 3);
        assert_eq!(listed["data"]["truncated"], truncated);
        let md = markdown(&file, &args);
        assert_eq!(md.lines().filter(|line| line.starts_with("- [")).count(), count);
        if truncated {
            let warnings = listed["meta"]["warnings"].as_array().unwrap();
            assert_eq!(warnings.len(), 1);
            let notice = warnings[0].as_str().unwrap();
            assert!(notice.contains(&count.to_string()));
            assert!(notice.contains('3'));
            assert!(notice.contains("--limit"));
            assert_eq!(md.lines().last().unwrap(), format!("> note: {notice}"));
        } else {
            assert!(listed["meta"].get("warnings").is_none());
            assert!(!md.contains("> note:"));
        }
    }
}
