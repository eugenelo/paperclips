use crate::cli::AddArgs;
use crate::error::{AppError, AppResult};
use crate::output::{self, Meta};
use crate::store;
use crate::{CutRecord, compute_id, format_timestamp, normalize_where, resolve_agent};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::io::{IsTerminal, Read};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
pub struct AddData {
    pub changed: bool,
    pub record: CutRecord,
}

/// Append one cut to the discovered log, first keeping a linked worktree's shared
/// `.scratch/` log ignored; a duplicate returns the existing record.
/// @param args Parsed `add` arguments.
/// @param file Optional explicit log path.
/// @param pretty Whether to pretty-print JSON.
/// @param now Record timestamp.
/// @return Zero on success, including dry runs and duplicates.
/// @throws AppError On discovery, invalid input, ignore setup, log IO, or output failure.
pub fn run(args: AddArgs, file: Option<PathBuf>, pretty: bool, now: Timestamp) -> AppResult<i32> {
    let resolved = store::discover(file)?;
    let text = read_text(args.text)?;
    if text.trim().is_empty() {
        return Err(AppError::invalid_input(
            "papercut text cannot be empty or whitespace-only",
            "Pass non-empty TEXT or pipe it on stdin.",
        ));
    }
    if text.len() > 10_000 {
        return Err(AppError::invalid_input(
            format!(
                "papercut text is {} bytes; the maximum is 10000",
                text.len()
            ),
            "Shorten the papercut text to at most 10000 UTF-8 bytes.",
        ));
    }
    if !args.force {
        if let Some(pattern) = crate::secrets::scan(&text) {
            return Err(AppError::secret_detected(pattern));
        }
    }
    if args
        .agent
        .as_deref()
        .is_some_and(|agent| agent.trim().is_empty())
    {
        return Err(AppError::invalid_input(
            "agent name cannot be empty or whitespace-only",
            "Pass a non-empty --agent NAME or omit the flag.",
        ));
    }
    let (agent, source) = resolve_agent(args.agent);
    if agent.trim().is_empty() {
        return Err(AppError::invalid_input(
            "agent name cannot be whitespace-only",
            "Pass a non-empty --agent NAME or set PAPERCUTS_AGENT.",
        ));
    }
    let mut tags = args.tags;
    tags.sort();
    let ts = format_timestamp(now);
    let where_loc = normalize_where(args.where_loc);
    let record = CutRecord {
        kind: "cut".into(),
        id: compute_id(&ts, &agent, &text, args.severity, &tags),
        ts,
        agent,
        text,
        tags,
        severity: args.severity,
        cwd: store::repo_relative(
            resolved.repo.as_deref(),
            &std::env::current_dir()
                .map_err(|error| AppError::from_io(error, std::path::Path::new(".")))?,
        ),
        repo: resolved.repo.as_ref().map(|_| ".".to_string()),
        where_loc,
    };

    let mut warnings = Vec::new();
    let (changed, record) = if args.dry_run {
        warnings.push("dry run; no record appended".into());
        (false, record)
    } else {
        if let Some(main) = resolved.main_checkout.as_deref()
            && let Some(warning) = store::ignore_scratch(main, &resolved.path)?
        {
            warnings.push(warning);
        }
        store::with_exclusive(&resolved.path, true, |log| {
            let bytes = store::read_bytes(log, &resolved.path)?;
            if let Some(existing) = store::fold_bytes(&bytes)
                .items
                .into_iter()
                .find(|item| item.cut.id == record.id)
            {
                return Ok((false, existing.cut));
            }
            store::append_json(log, &resolved.path, &bytes, &record)?;
            Ok((true, record))
        })?
    };
    if !changed && !args.dry_run {
        warnings.push("duplicate papercut; existing record returned".into());
    }
    let mut meta = Meta::new();
    meta.file = Some(resolved.path.to_string_lossy().into_owned());
    meta.agent_source = Some(source.into());
    meta.warnings = warnings;
    output::write_success(AddData { changed, record }, pretty, meta)
        .map_err(|error| AppError::from_io(error, std::path::Path::new("stdout")))?;
    Ok(0)
}

fn read_text(text: Option<String>) -> AppResult<String> {
    let use_stdin =
        text.as_deref() == Some("-") || (text.is_none() && !std::io::stdin().is_terminal());
    let mut text = if use_stdin {
        let mut input = Vec::new();
        std::io::stdin()
            .lock()
            .read_to_end(&mut input)
            .map_err(|error| AppError::from_io(error, std::path::Path::new("stdin")))?;
        String::from_utf8(input).map_err(|_| {
            AppError::invalid_input(
                "papercut text from stdin is not valid UTF-8",
                "Pipe UTF-8 text to `papercuts add -`.",
            )
        })?
    } else {
        text.ok_or_else(|| {
            AppError::invalid_argument(
                "add requires TEXT when stdin is a terminal",
                "Run `papercuts add \"what went wrong\"` or pipe text to `papercuts add -`.",
            )
        })?
    };
    while text.ends_with('\n') || text.ends_with('\r') {
        text.pop();
    }
    Ok(text)
}
