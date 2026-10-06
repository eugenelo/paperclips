use assert_cmd::Command;
use papercuts::commands::add::AddData;
use papercuts::commands::doctor::DoctorData;
use papercuts::commands::list::ListData;
use papercuts::commands::resolve::ResolveData;
use papercuts::error::exit_code_map;
use papercuts::output::{ErrorEnvelope, SuccessEnvelope};
use papercuts::{ItemStatus, Severity, compute_id};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Barrier};
use std::thread;
use tempfile::TempDir;

const NOW: &str = "2026-07-09T18:30:00.123456Z";

fn command() -> Command {
    let mut command = assert_cmd::cargo::cargo_bin_cmd!("papercuts");
    command
        .env("PAPERCUTS_NOW", NOW)
        .env_remove("PAPERCUTS_FILE")
        .env_remove("PAPERCUTS_AGENT")
        .env_remove("CLAUDECODE");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("CODEX_")
            || key.to_string_lossy().starts_with("CURSOR_")
        {
            command.env_remove(key);
        }
    }
    command
}

fn run(args: &[&str]) -> std::process::Output {
    command().args(args).output().unwrap()
}

fn run_file(file: &Path, args: &[&str]) -> std::process::Output {
    command()
        .arg("--file")
        .arg(file)
        .args(args)
        .output()
        .unwrap()
}

fn temp_has_git_ancestor(temp: &TempDir) -> bool {
    temp.path()
        .ancestors()
        .any(|ancestor| ancestor.join(".git").exists())
}

fn success<T: DeserializeOwned>(output: &std::process::Output) -> SuccessEnvelope<T> {
    assert!(
        output.status.success(),
        "status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    serde_json::from_slice(&output.stdout).unwrap()
}

fn error(output: &std::process::Output, exit: i32, code: &str) -> ErrorEnvelope {
    assert_eq!(output.status.code(), Some(exit));
    assert!(output.stdout.is_empty());
    let envelope: ErrorEnvelope = serde_json::from_slice(&output.stderr).unwrap();
    assert!(!envelope.ok);
    assert_eq!(envelope.error.code, code);
    assert!(!envelope.error.suggested_fix.is_empty());
    assert_eq!(envelope.meta.contract, 1);
    envelope
}

fn add(file: &Path, text: &str) -> SuccessEnvelope<AddData> {
    let output = run_file(file, &["add", text, "--agent", "tester"]);
    success(&output)
}

#[test]
fn every_command_success_envelope_deserializes() {
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("cuts.jsonl");
    let added = add(&file, "first cut");
    assert!(added.ok);
    assert!(added.data.changed);
    assert_eq!(added.data.record.ts, "2026-07-09T18:30:00.123Z");
    assert_eq!(added.meta.agent_source.as_deref(), Some("flag"));

    let listed: SuccessEnvelope<ListData> = success(&run_file(&file, &["list", "--status", "all"]));
    assert_eq!(listed.data.count, 1);

    let resolved: SuccessEnvelope<ResolveData> = success(&run_file(
        &file,
        &[
            "resolve",
            &added.data.record.id,
            "--agent",
            "fixer",
            "--note",
            "fixed",
        ],
    ));
    assert!(resolved.data.changed);
    assert_eq!(resolved.data.record.status, ItemStatus::Resolved);
    assert_eq!(
        resolved.data.record.resolution.unwrap().note.as_deref(),
        Some("fixed")
    );

    let doctor_output = run_file(&file, &["doctor"]);
    let doctor: SuccessEnvelope<DoctorData> = success(&doctor_output);
    assert!(doctor.data.healthy);
    assert_eq!(doctor.data.checked_lines, 2);

    let schema: SuccessEnvelope<Value> = success(&run(&["schema"]));
    assert_eq!(schema.data["contract"], 1);
    assert_eq!(schema.data["exit_codes"]["74"], "I/O error");
    assert_eq!(schema.data["commands"]["doctor"]["read_only"], true);

    let expected = serde_json::to_value(exit_code_map()).unwrap();
    assert_eq!(schema.data["exit_codes"], expected);
}

#[test]
fn add_stdin_validation_duplicate_and_exact_id() {
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("cuts.jsonl");
    let mut stdin = command();
    let output = stdin
        .arg("--file")
        .arg(&file)
        .args([
            "add",
            "-",
            "--agent",
            "tester",
            "--severity",
            "major",
            "--tag",
            "z",
            "--tag",
            "a",
        ])
        .write_stdin("ouch\n")
        .output()
        .unwrap();
    let first: SuccessEnvelope<AddData> = success(&output);
    assert_eq!(first.data.record.id, "pc_6d26611bad4c");
    assert_eq!(first.data.record.tags, ["a", "z"]);

    let second: SuccessEnvelope<AddData> = success(
        &command()
            .arg("--file")
            .arg(&file)
            .args([
                "add",
                "ouch",
                "--agent",
                "tester",
                "--severity",
                "major",
                "--tag",
                "z",
                "--tag",
                "a",
            ])
            .output()
            .unwrap(),
    );
    assert!(!second.data.changed);
    assert_eq!(second.meta.warnings.len(), 1);
    assert_eq!(std::fs::read_to_string(&file).unwrap().lines().count(), 1);

    let blank = command()
        .arg("--file")
        .arg(&file)
        .arg("add")
        .write_stdin(" \n")
        .output()
        .unwrap();
    error(&blank, 65, "invalid_input");
    let large = "x".repeat(10_001);
    error(&run_file(&file, &["add", &large]), 65, "invalid_input");
}

#[test]
fn list_filters_sorts_limits_since_and_markdown() {
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("cuts.jsonl");
    let cases = [
        ("2026-07-01T00:00:00Z", "old blocker", "blocker", "ops"),
        ("2026-07-09T17:00:00Z", "new minor", "minor", "shell"),
        ("2026-07-09T18:00:00Z", "new major", "major", "ops"),
    ];
    for (now, text, severity, tag) in cases {
        let output = command()
            .env("PAPERCUTS_NOW", now)
            .arg("--file")
            .arg(&file)
            .args([
                "add",
                text,
                "--agent",
                "tester",
                "--severity",
                severity,
                "--tag",
                tag,
            ])
            .output()
            .unwrap();
        success::<AddData>(&output);
    }
    let limited: SuccessEnvelope<ListData> = success(&run_file(&file, &["list", "--limit", "1"]));
    assert_eq!(limited.data.items[0].cut.text, "old blocker");
    assert_eq!(limited.data.total, 3);
    assert!(limited.data.truncated);

    let since: SuccessEnvelope<ListData> = success(
        &command()
            .env("PAPERCUTS_NOW", "2026-07-09T19:00:00Z")
            .arg("--file")
            .arg(&file)
            .args(["list", "--since", "2h", "--tag", "ops"])
            .output()
            .unwrap(),
    );
    assert_eq!(since.data.items.len(), 1);
    assert_eq!(since.data.items[0].cut.text, "new major");

    let markdown = run_file(&file, &["list", "--format", "md", "--severity", "major"]);
    assert!(markdown.status.success());
    assert!(markdown.stderr.is_empty());
    let markdown = String::from_utf8(markdown.stdout).unwrap();
    assert!(markdown.starts_with("## Major\n"));
    assert!(markdown.contains("new major — tester"));
    assert!(serde_json::from_str::<Value>(&markdown).is_err());
    error(
        &run_file(&file, &["list", "--since", "2026-07-09"]),
        2,
        "invalid_argument",
    );
}

#[test]
fn list_sorts_rfc3339_offsets_by_instant_not_text() {
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("offsets.jsonl");
    let earlier = json!({"kind":"cut","id":"pc_111111111111","ts":"2026-07-09T10:00:00+02:00","agent":"a","text":"earlier","tags":[],"severity":"minor","cwd":"/tmp","repo":null});
    let later = json!({"kind":"cut","id":"pc_222222222222","ts":"2026-07-09T09:00:00Z","agent":"a","text":"later","tags":[],"severity":"minor","cwd":"/tmp","repo":null});
    std::fs::write(&file, format!("{earlier}\n{later}\n")).unwrap();
    let listed: SuccessEnvelope<ListData> = success(&run_file(&file, &["list"]));
    assert_eq!(listed.data.items[0].cut.text, "later");
}

#[test]
fn resolve_prefix_errors_and_idempotence_are_structured() {
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("cuts.jsonl");
    let added = add(&file, "resolve me");
    let id = added.data.record.id;
    let prefix = &id[3..7];
    let first: SuccessEnvelope<ResolveData> = success(&run_file(
        &file,
        &["resolve", &prefix.to_ascii_uppercase(), "--agent", "fixer"],
    ));
    assert!(first.data.changed);
    let second: SuccessEnvelope<ResolveData> =
        success(&run_file(&file, &["resolve", &id, "--agent", "fixer"]));
    assert!(!second.data.changed);
    assert_eq!(second.meta.warnings, ["already resolved"]);

    error(&run_file(&file, &["resolve", "abc"]), 2, "invalid_argument");
    error(&run_file(&file, &["resolve", "deadbeef"]), 66, "not_found");

    let ambiguous = temp.path().join("ambiguous.jsonl");
    let lines = ["pc_abcd00000000", "pc_abcd11111111"]
        .map(|id| {
            json!({"kind":"cut","id":id,"ts":"2026-07-09T00:00:00.000Z","agent":"a","text":id,"tags":[],"severity":"minor","cwd":"/tmp","repo":null}).to_string()
        })
        .join("\n")
        + "\n";
    std::fs::write(&ambiguous, lines).unwrap();
    let envelope = error(
        &run_file(&ambiguous, &["resolve", "abcd"]),
        65,
        "ambiguous_id",
    );
    assert_eq!(
        envelope.error.details["candidates"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn structured_error_exit_matrix_and_help_exceptions() {
    let temp = TempDir::new().unwrap();
    let missing = temp.path().join("missing.jsonl");
    error(&run_file(&missing, &["list"]), 66, "not_found");
    error(&run(&["list", "--format", "jsonl"]), 2, "invalid_argument");
    error(
        &command()
            .env("PAPERCUTS_NOW", "not-a-time")
            .args(["schema"])
            .output()
            .unwrap(),
        78,
        "config_error",
    );
    error(
        &run_file(&missing, &["add", " ", "--agent", "tester"]),
        65,
        "invalid_input",
    );
    let invalid_utf8 = command()
        .arg("--file")
        .arg(&missing)
        .args(["add", "-", "--agent", "tester"])
        .write_stdin(vec![0xff])
        .output()
        .unwrap();
    error(&invalid_utf8, 65, "invalid_input");
    let directory_error = run_file(temp.path(), &["list"]);
    error(&directory_error, 74, "io_error");

    let help = run(&["--help"]);
    assert!(help.status.success());
    assert!(help.stderr.is_empty());
    assert!(String::from_utf8_lossy(&help.stdout).contains("Usage:"));
    let version = run(&["--version"]);
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout),
        "paper-core 0.1.0\n"
    );
}

#[test]
fn agent_resolution_order_and_sources_are_pinned() {
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("unused.jsonl");
    let invoke = |command: &mut Command| -> SuccessEnvelope<AddData> {
        success(
            &command
                .arg("--file")
                .arg(&file)
                .args(["add", "x", "--dry-run"])
                .output()
                .unwrap(),
        )
    };

    let default = invoke(&mut command());
    assert_eq!(default.data.record.agent, "unknown");
    assert_eq!(default.meta.agent_source.as_deref(), Some("default"));

    let claude = invoke(command().env("CLAUDECODE", "1"));
    assert_eq!(claude.data.record.agent, "claude-code");
    assert_eq!(claude.meta.agent_source.as_deref(), Some("detected"));

    let codex = invoke(command().env("CODEX_TEST", "1").env("CURSOR_TEST", "1"));
    assert_eq!(codex.data.record.agent, "codex");

    let cursor = invoke(command().env("CURSOR_TEST", "1"));
    assert_eq!(cursor.data.record.agent, "cursor");

    let env = invoke(
        command()
            .env("PAPERCUTS_AGENT", "from-env")
            .env("CLAUDECODE", "1"),
    );
    assert_eq!(env.data.record.agent, "from-env");
    assert_eq!(env.meta.agent_source.as_deref(), Some("env"));

    let flag: SuccessEnvelope<AddData> = success(
        &command()
            .env("PAPERCUTS_AGENT", "from-env")
            .arg("--file")
            .arg(&file)
            .args(["add", "x", "--agent", "from-flag", "--dry-run"])
            .output()
            .unwrap(),
    );
    assert_eq!(flag.data.record.agent, "from-flag");
    assert_eq!(flag.meta.agent_source.as_deref(), Some("flag"));
    assert!(!file.exists());
}

#[test]
fn mutation_dry_runs_do_not_write() {
    let temp = TempDir::new().unwrap();
    let dry_add = temp.path().join("nested/cuts.jsonl");
    let added: SuccessEnvelope<AddData> = success(&run_file(
        &dry_add,
        &["add", "preview", "--agent", "a", "--dry-run"],
    ));
    assert!(!added.data.changed);
    assert!(!dry_add.exists());

    let file = temp.path().join("cuts.jsonl");
    let id = add(&file, "resolve preview").data.record.id;
    let before = std::fs::read(&file).unwrap();
    let resolved: SuccessEnvelope<ResolveData> = success(&run_file(
        &file,
        &["resolve", &id, "--agent", "a", "--dry-run"],
    ));
    assert!(!resolved.data.changed);
    assert_eq!(resolved.data.record.status, ItemStatus::Resolved);
    assert_eq!(std::fs::read(&file).unwrap(), before);
}

#[cfg(unix)]
#[test]
fn permission_denied_is_exit_77() {
    use std::os::unix::fs::PermissionsExt;
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("cuts.jsonl");
    std::fs::write(&file, "{}\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
    let output = run_file(&file, &["list"]);
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    error(&output, 77, "permission_denied");
}

#[test]
fn lock_timeout_is_retryable_exit_75() {
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("cuts.jsonl");
    add(&file, "locked");
    let locked = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&file)
        .unwrap();
    locked.lock().unwrap();
    let output = run_file(&file, &["list"]);
    locked.unlock().unwrap();
    let envelope = error(&output, 75, "lock_timeout");
    assert!(envelope.error.retryable);
}

#[test]
fn doctor_reports_all_core_findings_and_recomputed_ids() {
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("cuts.jsonl");
    let good = add(&file, "valid").data.record;
    let good_line = std::fs::read_to_string(&file).unwrap();
    let bad_id = json!({"kind":"cut","id":"pc_000000000000","ts":good.ts,"agent":"tester","text":"bad","tags":[],"severity":"minor","cwd":"/tmp","repo":null});
    let mut writer = OpenOptions::new().append(true).open(&file).unwrap();
    writeln!(writer, "{good_line}{}", bad_id).unwrap();
    writeln!(writer, "{{\"kind\":\"future\"}}").unwrap();
    writeln!(writer, "{{\"kind\":\"resolve\",\"id\":\"pc_deadbeef0000\",\"ts\":\"2026-07-09T00:00:00.000Z\",\"agent\":\"a\",\"note\":null}}").unwrap();
    writeln!(writer, "<<<<<<< HEAD").unwrap();
    write!(writer, "{{\"kind\":").unwrap();
    drop(writer);
    let output = run_file(&file, &["doctor"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stderr.is_empty());
    let envelope: SuccessEnvelope<DoctorData> = serde_json::from_slice(&output.stdout).unwrap();
    let kinds: Vec<_> = envelope
        .data
        .findings
        .iter()
        .map(|finding| finding.kind.as_str())
        .collect();
    for kind in [
        "duplicate_cut",
        "id_conflict",
        "unknown_kind",
        "orphan_resolve",
        "conflict_marker",
        "torn_line",
    ] {
        assert!(kinds.contains(&kind), "missing {kind}: {kinds:?}");
    }
    assert!(!envelope.data.healthy);
}

#[test]
fn torn_tail_self_heals_on_add() {
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("cuts.jsonl");
    std::fs::write(&file, b"{\"kind\":\"cut\"").unwrap();
    let added = add(&file, "after tear");
    assert!(added.data.changed);
    let bytes = std::fs::read(&file).unwrap();
    assert!(bytes.ends_with(b"\n"));
    assert_eq!(bytes.split(|byte| *byte == b'\n').count(), 3);
    let listed: SuccessEnvelope<ListData> = success(&run_file(&file, &["list"]));
    assert_eq!(listed.data.items.len(), 1);
    assert_eq!(listed.data.items[0].cut.text, "after tear");
    assert!(
        listed
            .meta
            .warnings
            .iter()
            .any(|warning| warning.contains("malformed"))
    );
}

#[test]
fn doctor_finding_counts_match_fold_bytes_warning_counts() {
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("cuts.jsonl");
    let valid_id = compute_id(
        "2026-07-09T00:00:00.000Z",
        "a",
        "valid",
        Severity::Minor,
        &[],
    );
    let malformed = json!({
        "kind": "cut",
        "id": "pc_000000000000",
        "ts": "not-a-time",
        "agent": "a",
        "text": "malformed",
        "tags": [],
        "severity": "minor",
        "cwd": "/tmp",
        "repo": null
    })
    .to_string();
    let valid = json!({
        "kind": "cut",
        "id": valid_id,
        "ts": "2026-07-09T00:00:00.000Z",
        "agent": "a",
        "text": "valid",
        "tags": [],
        "severity": "minor",
        "cwd": "/tmp",
        "repo": null
    })
    .to_string();
    let orphan = json!({
        "kind": "resolve",
        "id": "pc_deadbeef0000",
        "ts": "2026-07-09T00:00:00.000Z",
        "agent": "a",
        "note": null
    })
    .to_string();
    let unknown = json!({"kind": "future"}).to_string();
    let fixture = format!("{malformed}\n{valid}\n{orphan}\n{valid}\n{unknown}\n{{\"kind\":");
    std::fs::write(&file, fixture).unwrap();

    let folded = papercuts::store::fold_bytes(&std::fs::read(&file).unwrap());
    let doctor_output = run_file(&file, &["doctor"]);
    assert_eq!(doctor_output.status.code(), Some(1));
    assert!(doctor_output.stderr.is_empty());
    let doctor: SuccessEnvelope<DoctorData> =
        serde_json::from_slice(&doctor_output.stdout).unwrap();

    let fold_counts = fold_warning_counts(&folded.warnings);
    let doctor_counts = doctor_finding_counts(&doctor.data.findings);
    let expected: HashMap<String, usize> = [
        ("malformed", 1),
        ("unknown", 1),
        ("duplicate_cut", 1),
        ("orphan_resolve", 1),
        ("torn", 1),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    assert_eq!(
        fold_counts, expected,
        "fold warnings: {:?}",
        folded.warnings
    );
    assert_eq!(
        doctor_counts, expected,
        "doctor findings: {:?}",
        doctor.data.findings
    );
}

fn fold_warning_counts(warnings: &[String]) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for warning in warnings {
        let parts: Vec<_> = warning.splitn(3, ' ').collect();
        let count: usize = parts[1].parse().unwrap();
        let label = parts[2].trim_end_matches('s');
        let key = if label.starts_with("malformed line") {
            "malformed"
        } else if label.starts_with("torn final line") {
            "torn"
        } else if label.starts_with("unknown event") {
            "unknown"
        } else if label.starts_with("duplicate cut") {
            "duplicate_cut"
        } else if label.starts_with("duplicate resolve") {
            "duplicate_resolve"
        } else if label.starts_with("orphan resolve") {
            "orphan_resolve"
        } else {
            panic!("unknown fold warning label: {label}")
        };
        counts.insert(key.to_string(), count);
    }
    counts
}

fn doctor_finding_counts(
    findings: &[papercuts::commands::doctor::Finding],
) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for finding in findings {
        let key = match finding.kind.as_str() {
            "malformed" => "malformed",
            "torn_line" => "torn",
            "unknown_kind" => "unknown",
            "duplicate_cut" => "duplicate_cut",
            "orphan_resolve" => "orphan_resolve",
            _ => continue,
        };
        *counts.entry(key.to_string()).or_insert(0) += 1;
    }
    counts
}

#[test]
fn discovery_precedence_virtual_empty_and_git_file_root() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("repo");
    let nested = root.join("a/b");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(root.join(".git"), "gitdir: elsewhere\n").unwrap();
    let env_file = temp.path().join("env.jsonl");
    let flag_file = temp.path().join("flag.jsonl");

    let walk: SuccessEnvelope<AddData> = success(
        &command()
            .current_dir(&nested)
            .args(["add", "x", "--agent", "a", "--dry-run"])
            .output()
            .unwrap(),
    );
    let canonical_root = root.canonicalize().unwrap();
    assert_eq!(
        walk.meta.file.as_deref(),
        Some(canonical_root.join(".papercuts.jsonl").to_str().unwrap())
    );
    let empty_env: SuccessEnvelope<AddData> = success(
        &command()
            .current_dir(&nested)
            .env("PAPERCUTS_FILE", "")
            .args(["add", "x", "--agent", "a", "--dry-run"])
            .output()
            .unwrap(),
    );
    assert_eq!(empty_env.meta.file, walk.meta.file);

    let env: SuccessEnvelope<AddData> = success(
        &command()
            .current_dir(&nested)
            .env("PAPERCUTS_FILE", &env_file)
            .args(["add", "x", "--agent", "a", "--dry-run"])
            .output()
            .unwrap(),
    );
    assert_eq!(env.meta.file.as_deref(), Some(env_file.to_str().unwrap()));

    let flag: SuccessEnvelope<AddData> = success(
        &command()
            .current_dir(&nested)
            .env("PAPERCUTS_FILE", &env_file)
            .arg("--file")
            .arg(&flag_file)
            .args(["add", "x", "--agent", "a", "--dry-run"])
            .output()
            .unwrap(),
    );
    assert_eq!(flag.meta.file.as_deref(), Some(flag_file.to_str().unwrap()));

    let empty: SuccessEnvelope<ListData> =
        success(&command().current_dir(&nested).arg("list").output().unwrap());
    assert!(empty.data.items.is_empty());
    assert!(
        empty
            .meta
            .warnings
            .iter()
            .any(|warning| warning.contains("no papercuts file"))
    );

    if !temp_has_git_ancestor(&temp) {
        let outside = temp.path().join("outside");
        let home = temp.path().join("home");
        std::fs::create_dir_all(&outside).unwrap();
        let home_result: SuccessEnvelope<AddData> = success(
            &command()
                .current_dir(&outside)
                .env("HOME", &home)
                .args(["add", "x", "--agent", "a", "--dry-run"])
                .output()
                .unwrap(),
        );
        assert_eq!(
            home_result.meta.file.as_deref(),
            Some(home.join(".papercuts/log.jsonl").to_str().unwrap())
        );
        assert!(
            !home.exists(),
            "dry run must not create the home fallback directory"
        );
        let no_home = command()
            .current_dir(&outside)
            .env_remove("HOME")
            .arg("list")
            .output()
            .unwrap();
        error(&no_home, 78, "config_error");
    } else {
        eprintln!(
            "skipping home-fallback assertions because the temporary directory is inside a git checkout"
        );
    }
}

#[test]
fn fixed_clock_fresh_state_is_byte_deterministic_and_retry_is_duplicate_safe() {
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("cuts.jsonl");
    let first = run_file(&file, &["add", "same", "--agent", "tester"]);
    assert!(first.status.success());
    std::fs::remove_file(&file).unwrap();
    let fresh = run_file(&file, &["add", "same", "--agent", "tester"]);
    assert_eq!(first.stdout, fresh.stdout);
    let retry: SuccessEnvelope<AddData> =
        success(&run_file(&file, &["add", "same", "--agent", "tester"]));
    assert!(!retry.data.changed);
}

#[test]
fn eight_way_distinct_add_race_loses_no_lines() {
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("cuts.jsonl");
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|thread_id| {
            let file = file.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                for item in 0..4 {
                    let text = format!("thread-{thread_id}-item-{item}");
                    let output = run_file(&file, &["add", &text, "--agent", "race"]);
                    assert!(
                        output.status.success(),
                        "{}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    let contents = std::fs::read_to_string(&file).unwrap();
    assert_eq!(contents.lines().count(), 32);
    for line in contents.lines() {
        serde_json::from_str::<Value>(line).unwrap();
    }
}

#[test]
fn eight_way_identical_add_race_appends_once() {
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("cuts.jsonl");
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let file = file.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                let envelope: SuccessEnvelope<AddData> =
                    success(&run_file(&file, &["add", "identical", "--agent", "race"]));
                envelope.data.changed
            })
        })
        .collect();
    let changed = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .filter(|changed| *changed)
        .count();
    assert_eq!(changed, 1);
    assert_eq!(std::fs::read_to_string(&file).unwrap().lines().count(), 1);
}

#[test]
fn eight_way_resolve_race_appends_once() {
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("cuts.jsonl");
    let id = add(&file, "resolve race").data.record.id;
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let file = file.clone();
            let id = id.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                let envelope: SuccessEnvelope<ResolveData> =
                    success(&run_file(&file, &["resolve", &id, "--agent", "race"]));
                envelope.data.changed
            })
        })
        .collect();
    let changed = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .filter(|changed| *changed)
        .count();
    assert_eq!(changed, 1);
    assert_eq!(std::fs::read_to_string(&file).unwrap().lines().count(), 2);
}

#[test]
fn hash_length_prefix_and_tag_sort_are_pinned() {
    let a = compute_id(
        "2026-07-09T18:30:00.123Z",
        "tester",
        "ouch",
        Severity::Major,
        &["a".into(), "z".into()],
    );
    let b = compute_id(
        "2026-07-09T18:30:00.123Z",
        "tester",
        "ouc",
        Severity::Major,
        &["z".into(), "ha".into()],
    );
    let unsorted = compute_id(
        "2026-07-09T18:30:00.123Z",
        "tester",
        "ouch",
        Severity::Major,
        &["z".into(), "a".into()],
    );
    assert_eq!(a, "pc_6d26611bad4c");
    assert_eq!(a, unsorted);
    assert_ne!(a, b);
}

#[test]
fn env_papercuts_file_nonexistent_returns_not_found() {
    let temp = TempDir::new().unwrap();
    let missing = temp.path().join("missing.jsonl");
    let output = command()
        .env("PAPERCUTS_FILE", &missing)
        .arg("list")
        .output()
        .unwrap();
    error(&output, 66, "not_found");
}

#[test]
fn relative_file_resolves_against_cwd() {
    let temp = TempDir::new().unwrap();
    let output = command()
        .current_dir(temp.path())
        .arg("--file")
        .arg("rel/path.jsonl")
        .args(["add", "x", "--agent", "a", "--dry-run"])
        .output()
        .unwrap();
    let envelope: SuccessEnvelope<AddData> = success(&output);
    let temp_canonical = temp.path().canonicalize().unwrap();
    assert!(
        Path::new(envelope.meta.file.as_deref().unwrap()).starts_with(&temp_canonical),
        "meta.file = {:?}",
        envelope.meta.file
    );
}

#[test]
fn markdown_format_is_byte_deterministic() {
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("cuts.jsonl");
    let added = add(&file, "determinism");
    let first = run_file(&file, &["list", "--format", "md"]);
    assert!(first.status.success());
    assert!(!first.stdout.is_empty());
    let first_text = String::from_utf8_lossy(&first.stdout);
    assert!(first_text.contains("determinism"));
    assert!(first_text.contains(&added.data.record.id));
    let second = run_file(&file, &["list", "--format", "md"]);
    assert!(second.status.success());
    assert_eq!(first.stdout, second.stdout);
}

#[test]
fn doctor_reports_gitignored_finding() {
    let git_available = std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    if !git_available {
        return;
    }

    let temp = TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    assert!(
        std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("init")
            .output()
            .unwrap()
            .status
            .success()
    );
    std::fs::write(repo.join(".gitignore"), ".papercuts.jsonl\n").unwrap();

    let output = command()
        .current_dir(&repo)
        .args(["add", "gitignored cut", "--agent", "a"])
        .output()
        .unwrap();
    success::<AddData>(&output);

    let doctor_output = command().current_dir(&repo).arg("doctor").output().unwrap();
    assert_eq!(doctor_output.status.code(), Some(1));
    assert!(doctor_output.stderr.is_empty());
    let doctor: SuccessEnvelope<DoctorData> =
        serde_json::from_slice(&doctor_output.stdout).unwrap();
    assert!(!doctor.data.healthy);
    assert!(
        doctor
            .data
            .findings
            .iter()
            .any(|finding| finding.kind == "gitignored")
    );
}

/// Run Git for a fixture under an isolated configuration that ignores nothing by default.
/// @param home Isolated HOME and XDG_CONFIG_HOME root.
/// @param cwd Directory to run in.
/// @param args Git arguments.
/// @return Captured stdout as UTF-8.
fn fixture_git(home: &Path, cwd: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(cwd)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args(["-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid"])
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

/// A linked worktree appends to its main checkout's ignored `.scratch/` log, keeping earlier
/// entries and creating nothing in the worktree; main checkouts and bare-repository
/// worktrees keep the per-root default.
#[test]
fn linked_worktree_logs_land_in_main_checkout_scratch() {
    let git_available = std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    if !git_available {
        return;
    }
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let main = temp.path().join("main");
    std::fs::create_dir_all(&main).unwrap();
    fixture_git(&home, &main, &["init", "-q", "-b", "main"]);
    std::fs::write(main.join("README.md"), "fixture\n").unwrap();
    fixture_git(&home, &main, &["add", "README.md"]);
    fixture_git(&home, &main, &["commit", "-q", "-m", "fixture"]);
    let worktree = temp.path().join("main-task");
    fixture_git(&home, &main, &["worktree", "add", "-q", "-b", "dev-cycle/task", worktree.to_str().unwrap()]);
    let main = main.canonicalize().unwrap();
    let worktree = worktree.canonicalize().unwrap();
    let shared = main.join(".scratch/papercuts.jsonl");
    add(&shared, "earlier entry");
    let earlier = std::fs::read(&shared).unwrap();
    let nested = worktree.join("sub/dir");
    std::fs::create_dir_all(&nested).unwrap();
    let linked = |args: &[&str]| {
        command()
            .current_dir(&nested)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(args)
            .output()
            .unwrap()
    };

    let dry: SuccessEnvelope<AddData> = success(&linked(&["add", "dry", "--agent", "a", "--dry-run"]));
    assert_eq!(dry.meta.file.as_deref(), Some(shared.to_str().unwrap()));
    assert!(!main.join(".git/info/exclude").exists() || !std::fs::read_to_string(main.join(".git/info/exclude")).unwrap().contains(".scratch/"));

    let first: SuccessEnvelope<AddData> = success(&linked(&["add", "from worktree", "--agent", "a"]));
    assert_eq!(first.meta.file.as_deref(), Some(shared.to_str().unwrap()));
    assert!(first.meta.warnings.iter().any(|warning| warning.contains("added .scratch/ to")));
    assert_eq!(first.data.record.cwd, "sub/dir");
    assert_eq!(first.data.record.repo.as_deref(), Some("."));
    let second: SuccessEnvelope<AddData> = success(&linked(&["add", "again", "--agent", "a"]));
    assert!(second.meta.warnings.is_empty());

    let log = std::fs::read(&shared).unwrap();
    assert!(log.starts_with(&earlier));
    assert_eq!(log.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()).count(), 3);
    let exclude = std::fs::read_to_string(main.join(".git/info/exclude")).unwrap();
    assert_eq!(exclude.lines().filter(|line| *line == ".scratch/").count(), 1);
    assert_eq!(fixture_git(&home, &main, &["status", "--porcelain", "--untracked-files=all"]), "");
    assert_eq!(
        fixture_git(&home, &worktree, &["status", "--porcelain", "--untracked-files=all", "--ignored"]),
        ""
    );
    let listed: SuccessEnvelope<ListData> = success(&linked(&["list"]));
    assert_eq!(listed.data.items.len(), 3);

    let from_main: SuccessEnvelope<AddData> = success(
        &command().current_dir(&main).args(["add", "main", "--agent", "a", "--dry-run"]).output().unwrap(),
    );
    assert_eq!(from_main.meta.file.as_deref(), Some(main.join(".papercuts.jsonl").to_str().unwrap()));

    let bare = temp.path().join("bare.git");
    fixture_git(&home, temp.path(), &["clone", "-q", "--bare", main.to_str().unwrap(), bare.to_str().unwrap()]);
    let bare_worktree = temp.path().join("bare-task");
    fixture_git(&home, &bare, &["worktree", "add", "-q", bare_worktree.to_str().unwrap(), "main"]);
    let bare_worktree = bare_worktree.canonicalize().unwrap();
    let from_bare: SuccessEnvelope<AddData> = success(
        &command().current_dir(&bare_worktree).args(["add", "bare", "--agent", "a", "--dry-run"]).output().unwrap(),
    );
    assert_eq!(from_bare.meta.file.as_deref(), Some(bare_worktree.join(".papercuts.jsonl").to_str().unwrap()));
}

#[test]
fn error_envelope_matrix() {
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("cuts.jsonl");
    let missing = temp.path().join("missing.jsonl");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();

    let ambiguous = temp.path().join("ambiguous.jsonl");
    let lines = ["pc_abcd00000000", "pc_abcd11111111"]
        .map(|id| {
            json!({"kind":"cut","id":id,"ts":"2026-07-09T00:00:00.000Z","agent":"a","text":id,"tags":[],"severity":"minor","cwd":"/tmp","repo":null}).to_string()
        })
        .join("\n")
        + "\n";
    std::fs::write(&ambiguous, lines).unwrap();

    error(&run(&["list", "--format", "jsonl"]), 2, "invalid_argument");
    error(
        &run_file(&file, &["add", " ", "--agent", "tester"]),
        65,
        "invalid_input",
    );
    error(&run_file(&missing, &["list"]), 66, "not_found");
    if temp_has_git_ancestor(&temp) {
        eprintln!(
            "skipping HOME/config-78 assertion because the temporary directory is inside a git checkout"
        );
    } else {
        error(
            &command()
                .current_dir(&outside)
                .env("HOME", "")
                .arg("list")
                .output()
                .unwrap(),
            78,
            "config_error",
        );
    }
    error(
        &run_file(&ambiguous, &["resolve", "abcd"]),
        65,
        "ambiguous_id",
    );
}

/// Report omitted matching cuts in both formats, but not at the exact limit.
#[test]
fn list_truncation_is_visible_after_filtering() {
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("cuts.jsonl");
    for text in ["first", "second", "third"] {
        success::<AddData>(&run_file(&file, &["add", text, "--tag", "selected"]));
    }
    add(&file, "outside filter");
    for (limit, count, truncated) in [("0", 0, true), ("2", 2, true), ("3", 3, false)] {
        let args = ["list", "--tag", "selected", "--limit", limit];
        let listed: SuccessEnvelope<ListData> = success(&run_file(&file, &args));
        assert_eq!(listed.data.count, count);
        assert_eq!(listed.data.items.len(), count);
        assert_eq!(listed.data.total, 3);
        assert_eq!(listed.data.truncated, truncated);
        let md = run_file(&file, &[&args[..], &["--format", "md"]].concat());
        assert!(md.status.success());
        assert!(md.stderr.is_empty());
        let md = String::from_utf8(md.stdout).unwrap();
        assert_eq!(md.lines().filter(|line| line.starts_with("- [")).count(), count);
        let notice = listed.meta.warnings.iter().find(|warning| warning.contains("--limit"));
        assert_eq!(notice.is_some(), truncated);
        if let Some(notice) = notice {
            assert!(notice.contains(&count.to_string()));
            assert!(notice.contains('3'));
            assert!(md.lines().last().unwrap().contains("--limit") || count == 0);
            assert!(md.contains(&format!("> note: {notice}")));
        } else {
            assert!(!md.contains("> note:"));
        }
    }
}
