use paper_core::error::{AppError, AppResult};
use paper_core::output::{self, Meta};
use paper_core::store;
use paper_core::{ClipRecord, ClipStatus, Impact, compute_clip_id, format_timestamp, normalize_where, resolve_agent};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;

use crate::{OutputFormat, StatusFilter};

#[derive(Debug, Serialize, Deserialize)]
pub struct ClipAddData {
    pub changed: bool,
    pub record: ClipRecord,
}

pub fn add(
    text: Option<String>,
    agent: Option<String>,
    tags: Vec<String>,
    impact: Impact,
    dry_run: bool,
    force: bool,
    where_loc: Option<String>,
    file: Option<PathBuf>,
    pretty: bool,
    now: Timestamp,
) -> AppResult<i32> {
    let resolved = store::discover_clips(file)?;
    let text = if let Some(t) = text {
        t
    } else if std::io::stdin().is_terminal() {
        return Err(AppError::invalid_argument(
            "TEXT is required when stdin is not piped",
            "Pass TEXT as a positional argument or pipe it on stdin.",
        ));
    } else {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)
            .map_err(|e| AppError::from_io(e, std::path::Path::new("stdin")))?;
        buf
    };
    if text.trim().is_empty() {
        return Err(AppError::invalid_input(
            "clip text cannot be empty",
            "Pass non-empty TEXT.",
        ));
    }
    if !force {
        if let Some(pattern) = paper_core::secrets::scan(&text) {
            return Err(AppError::secret_detected(pattern));
        }
    }
    let (agent, _source) = resolve_agent(agent);
    let mut tags = tags;
    tags.sort();
    let ts = format_timestamp(now);
    let where_normalized = normalize_where(where_loc);
    let record = ClipRecord {
        kind: "clip".into(),
        id: compute_clip_id(&ts, &agent, &text, impact, &tags),
        ts,
        agent,
        text,
        tags,
        impact,
        where_loc: where_normalized,
        cwd: store::repo_relative(
            resolved.repo.as_deref(),
            &std::env::current_dir()
                .map_err(|e| AppError::from_io(e, std::path::Path::new(".")))?,
        ),
        repo: resolved.repo.as_ref().map(|_| ".".to_string()),

    };
    let mut warnings = Vec::new();
    let (changed, record) = if dry_run {
        warnings.push("dry run; no record appended".into());
        (false, record)
    } else {
        store::with_exclusive(&resolved.path, true, |log| {
            let bytes = store::read_bytes(log, &resolved.path)?;
            let folded = store::fold_clip_bytes(&bytes);
            let duplicate = folded.items.iter().any(|item| item.clip.id == record.id);
            if !duplicate {
                store::append_json(log, &resolved.path, &bytes, &record)?;
            }
            Ok((!duplicate, record))
        })?
    };
    let mut meta = Meta::new();
    meta.file = Some(resolved.path.to_string_lossy().into_owned());
    meta.warnings = warnings;
    output::write_success(ClipAddData { changed, record }, pretty, meta)
        .map_err(|e| AppError::from_io(e, std::path::Path::new("stdout")))?;
    Ok(0)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ClipListData {
    pub items: Vec<paper_core::ClipListItem>,
    pub count: usize,
    pub total: usize,
    pub truncated: bool,
}

/// List matching clips in the requested format, with visible truncation warnings.
/// @param tag Optional tag filter.
/// @param impact Optional impact filter.
/// @param status_filter Included lifecycle statuses.
/// @param limit Maximum number of matching clips to return.
/// @param format Markdown or JSON output.
/// @param where_loc Optional exact location filter.
/// @param file Optional explicit log path.
/// @param pretty Whether to pretty-print JSON.
/// @return Zero on success, including a missing log.
/// @throws AppError On discovery, log IO other than missing files, or output failure.
pub fn list(
    tag: Option<String>,
    impact: Option<Impact>,
    status_filter: StatusFilter,
    limit: usize,
    format: OutputFormat,
    where_loc: Option<String>,
    file: Option<PathBuf>,
    pretty: bool,
) -> AppResult<i32> {
    let resolved = store::discover_clips(file)?;
    let folded = match store::with_shared(&resolved.path, |log| {
        let bytes = store::read_bytes(log, &resolved.path)?;
        Ok(store::fold_clip_bytes(&bytes))
    }) {
        Ok(f) => f,
        Err(e) if e.code == "not_found" => {
            if format == OutputFormat::Md {
                write_markdown(&[], &[])?;
            } else {
                let meta = Meta::new();
                output::write_success(ClipListData { items: vec![], count: 0, total: 0, truncated: false }, pretty, meta)
                    .map_err(|e2| AppError::from_io(e2, std::path::Path::new("stdout")))?;
            }
            return Ok(0);
        }
        Err(e) => return Err(e),
    };
    let mut items: Vec<_> = folded.items.into_iter()
        .filter(|item| match status_filter {
            StatusFilter::Open => item.status == ClipStatus::Open,
            StatusFilter::Promoted => item.status == ClipStatus::Promoted,
            StatusFilter::Noted => item.status == ClipStatus::Noted,
            StatusFilter::All => true,
        })
        .filter(|item| tag.as_ref().is_none_or(|t| item.clip.tags.contains(t)))
        .filter(|item| impact.as_ref().is_none_or(|i| item.clip.impact == *i))
        .filter(|item| where_loc.as_ref().is_none_or(|w| item.clip.where_loc.as_deref() == Some(w.as_str())))
        .collect();
    let total = items.len();
    items.truncate(limit);
    let count = items.len();
    let truncated = total > count;
    let mut meta = Meta::new();
    if truncated {
        meta.warnings.push(format!(
            "showing {count} of {total} matching clips; use --limit {total} to see all"
        ));
    }
    if format == OutputFormat::Md {
        write_markdown(&items, &meta.warnings)?;
    } else {
        output::write_success(ClipListData { items, count, total, truncated }, pretty, meta)
            .map_err(|e| AppError::from_io(e, std::path::Path::new("stdout")))?;
    }
    Ok(0)
}

/// Render clips grouped by impact without allocating intermediate groups.
/// @param items Selected clips in log order.
/// @param warnings Notices to append after the listing.
/// @return Success after writing and flushing Markdown to stdout.
/// @throws AppError On stdout write or flush failure.
fn write_markdown(items: &[paper_core::ClipListItem], warnings: &[String]) -> AppResult<()> {
    let mut output = std::io::BufWriter::new(std::io::stdout().lock());
    let write = |output: &mut std::io::BufWriter<std::io::StdoutLock<'_>>| -> std::io::Result<()> {
        for impact in [Impact::Huge, Impact::Solid, Impact::Nice] {
            let mut matching = items.iter().filter(|item| item.clip.impact == impact).peekable();
            if matching.peek().is_none() {
                continue;
            }
            writeln!(output, "## {}", impact.as_str())?;
            for item in matching {
                write!(output, "- [{}] ", item.clip.id)?;
                for (index, line) in item.clip.text.lines().enumerate() {
                    if index > 0 {
                        write!(output, " ")?;
                    }
                    write!(output, "{line}")?;
                }
                write!(output, " — {}", impact.as_str())?;
                if let Some(where_loc) = &item.clip.where_loc {
                    write!(output, ", where: {where_loc}")?;
                }
                if item.status != ClipStatus::Open {
                    write!(output, ", {}", item.status.as_str())?;
                }
                writeln!(output)?;
            }
        }
        for warning in warnings {
            writeln!(output, "> note: {warning}")?;
        }
        output.flush()
    };
    write(&mut output).map_err(|error| AppError::from_io(error, std::path::Path::new("stdout")))
}

/// Append a promotion with the same agent precedence as clip creation.
/// @param id Clip ID or prefix.
/// @param agent Optional explicit agent, ahead of environment and detection.
/// @param file Optional explicit log path.
/// @param pretty Whether to pretty-print the response.
/// @param now Event timestamp.
/// @return Zero on success.
/// @throws AppError On discovery, lookup, log IO, or output failure.
pub fn promote(id: String, agent: Option<String>, file: Option<PathBuf>, pretty: bool, now: Timestamp) -> AppResult<i32> {
    let resolved = store::discover_clips(file)?;
    let ts = format_timestamp(now);
    let (agent, _) = resolve_agent(agent);
    store::with_exclusive(&resolved.path, false, |log| {
        let bytes = store::read_bytes(log, &resolved.path)?;
        let folded = store::fold_clip_bytes(&bytes);
        let item = folded.items.iter().find(|i| i.clip.id.starts_with(&id))
            .ok_or_else(|| AppError::not_found(
                format!("clip not found: {}", id),
                "Run `paperclip list` to find valid IDs.",
            ))?;
        if item.status == ClipStatus::Promoted {
            return Ok(());
        }
        let event = serde_json::json!({
            "kind": "promote",
            "id": item.clip.id,
            "ts": ts,
            "agent": agent,
        });
        store::append_json(log, &resolved.path, &bytes, &event)?;
        Ok(())
    })?;
    let meta = Meta::new();
    output::write_success(serde_json::json!({"ok": true, "id": id}), pretty, meta)
        .map_err(|e| AppError::from_io(e, std::path::Path::new("stdout")))?;
    Ok(0)
}

/// Append an attributed note without closing the clip.
/// @param id Clip ID or prefix.
/// @param text Follow-up observation.
/// @param agent Optional explicit agent, ahead of environment and detection.
/// @param file Optional explicit log path.
/// @param pretty Whether to pretty-print the response.
/// @param now Event timestamp.
/// @return Zero on success.
/// @throws AppError On discovery, lookup, log IO, or output failure.
pub fn note(id: String, text: String, agent: Option<String>, file: Option<PathBuf>, pretty: bool, now: Timestamp) -> AppResult<i32> {
    let resolved = store::discover_clips(file)?;
    let ts = format_timestamp(now);
    let (agent, _) = resolve_agent(agent);
    store::with_exclusive(&resolved.path, false, |log| {
        let bytes = store::read_bytes(log, &resolved.path)?;
        let folded = store::fold_clip_bytes(&bytes);
        let item = folded.items.iter().find(|i| i.clip.id.starts_with(&id))
            .ok_or_else(|| AppError::not_found(
                format!("clip not found: {}", id),
                "Run `paperclip list` to find valid IDs.",
            ))?;
        let event = serde_json::json!({
            "kind": "note",
            "id": item.clip.id,
            "ts": ts,
            "text": text,
            "agent": agent,
        });
        store::append_json(log, &resolved.path, &bytes, &event)?;
        Ok(())
    })?;
    let meta = Meta::new();
    output::write_success(serde_json::json!({"ok": true, "id": id}), pretty, meta)
        .map_err(|e| AppError::from_io(e, std::path::Path::new("stdout")))?;
    Ok(0)
}

pub fn top(limit: usize, file: Option<PathBuf>, pretty: bool) -> AppResult<i32> {
    let resolved = store::discover_clips(file)?;
    let folded = match store::with_shared(&resolved.path, |log| {
        let bytes = store::read_bytes(log, &resolved.path)?;
        Ok(store::fold_clip_bytes(&bytes))
    }) {
        Ok(f) => f,
        Err(e) if e.code == "not_found" => {
            let meta = Meta::new();
            output::write_success(serde_json::json!({"items": [], "count": 0}), pretty, meta)
                .map_err(|e2| AppError::from_io(e2, std::path::Path::new("stdout")))?;
            return Ok(0);
        }
        Err(e) => return Err(e),
    };
    use std::collections::BTreeMap;
    let mut by_tag: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_where: BTreeMap<String, usize> = BTreeMap::new();
    for item in &folded.items {
        if item.status == ClipStatus::Open {
            for tag in &item.clip.tags {
                *by_tag.entry(tag.clone()).or_insert(0) += 1;
            }
            if let Some(ref w) = item.clip.where_loc {
                *by_where.entry(w.clone()).or_insert(0) += 1;
            }
        }
    }
    let top_tags: Vec<_> = by_tag.into_iter().rev().take(limit).collect();
    let top_wheres: Vec<_> = by_where.into_iter().rev().take(limit).collect();
    let meta = Meta::new();
    output::write_success(serde_json::json!({
        "by_tag": top_tags,
        "by_where": top_wheres,
        "total_open": folded.items.iter().filter(|i| i.status == ClipStatus::Open).count(),
    }), pretty, meta)
        .map_err(|e| AppError::from_io(e, std::path::Path::new("stdout")))?;
    Ok(0)
}

pub fn doctor(file: Option<PathBuf>, pretty: bool) -> AppResult<i32> {
    let resolved = store::discover_clips(file)?;
    let folded = match store::with_shared(&resolved.path, |log| {
        let bytes = store::read_bytes(log, &resolved.path)?;
        Ok(store::fold_clip_bytes(&bytes))
    }) {
        Ok(f) => f,
        Err(e) if e.code == "not_found" => {
            let meta = Meta::new();
            output::write_success(serde_json::json!({"healthy": true, "findings": [], "checked_lines": 0}), pretty, meta)
                .map_err(|e2| AppError::from_io(e2, std::path::Path::new("stdout")))?;
            return Ok(0);
        }
        Err(e) => return Err(e),
    };
    let mut findings = Vec::new();
    for w in &folded.warnings {
        findings.push(serde_json::json!({"kind": "warning", "message": w}));
    }
    let healthy = findings.is_empty();
    let meta = Meta::new();
    output::write_success(serde_json::json!({
        "healthy": healthy,
        "findings": findings,
        "total_clips": folded.items.len(),
    }), pretty, meta)
        .map_err(|e| AppError::from_io(e, std::path::Path::new("stdout")))?;
    Ok(i32::from(!healthy))
}
