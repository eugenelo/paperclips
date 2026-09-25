use crate::cli::{ListArgs, OutputFormat, StatusFilter};
use crate::error::{AppError, AppResult};
use crate::output::{self, Meta};
use crate::store;
use crate::{ItemStatus, ListItem, Severity, parse_since};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
pub struct ListData {
    pub items: Vec<ListItem>,
    pub count: usize,
    pub total: usize,
    pub truncated: bool,
}

/// List matching cuts and make any limit-induced omission visible in both formats.
/// @param args Filters, limit, and requested output format.
/// @param file Optional explicit log path.
/// @param pretty Whether to pretty-print JSON.
/// @param now Reference timestamp for relative date filters.
/// @return Zero on success.
/// @throws AppError On discovery, filtering, log IO, or output failure.
pub fn run(args: ListArgs, file: Option<PathBuf>, pretty: bool, now: Timestamp) -> AppResult<i32> {
    let resolved = store::discover(file)?;
    let mut warnings = Vec::new();
    let folded = match store::with_shared(&resolved.path, |log| {
        let bytes = store::read_bytes(log, &resolved.path)?;
        Ok(store::fold_bytes(&bytes))
    }) {
        Ok(folded) => folded,
        Err(error) if error.code == "not_found" && error.exit_code == 66 => {
            if resolved.explicit {
                return Err(AppError::not_found(
                    format!("papercuts file not found: {}", resolved.path.display()),
                    "Pass an existing --file PATH or run `papercuts add` to create a discovered default file.",
                ));
            }
            warnings.push("no papercuts file yet; papercuts add creates it".into());
            store::FoldResult::default()
        }
        Err(error) => return Err(error),
    };
    warnings.extend(folded.warnings);
    let since = args
        .since
        .as_deref()
        .map(|value| parse_since(value, now))
        .transpose()?;
    let mut items: Vec<_> = folded
        .items
        .into_iter()
        .filter(|item| match args.status {
            StatusFilter::Open => item.status == ItemStatus::Open,
            StatusFilter::Resolved => item.status == ItemStatus::Resolved,
            StatusFilter::All => true,
        })
        .filter(|item| {
            args.agent
                .as_ref()
                .is_none_or(|agent| &item.cut.agent == agent)
        })
        .filter(|item| {
            args.tag
                .as_ref()
                .is_none_or(|tag| item.cut.tags.contains(tag))
        })
        .filter(|item| {
            args.severity
                .is_none_or(|severity| item.cut.severity == severity)
        })
        .filter(|item| {
            since.is_none_or(|threshold| {
                item.cut
                    .ts
                    .parse::<Timestamp>()
                    .is_ok_and(|timestamp| timestamp >= threshold)
            })
        })
        .filter(|item| {
            args.where_loc
                .as_ref()
                .is_none_or(|filter| item.cut.where_loc.as_ref().is_some_and(|loc| loc == filter))
        })
        .collect();
    let total = items.len();
    items.truncate(args.limit);
    let data = ListData {
        count: items.len(),
        total,
        truncated: total > items.len(),
        items,
    };
    if data.truncated {
        warnings.push(format!(
            "showing {} of {} matching cuts; use --limit {} to see all",
            data.count, data.total, data.total
        ));
    }
    if data.items.is_empty() {
        warnings.push("no papercuts matched; try --status all or broader filters".into());
    }
    if args.format == OutputFormat::Md {
        write_markdown(&data.items, &warnings)?;
    } else {
        let mut meta = Meta::new();
        meta.file = Some(resolved.path.to_string_lossy().into_owned());
        meta.warnings = warnings;
        output::write_success(data, pretty, meta)
            .map_err(|error| AppError::from_io(error, std::path::Path::new("stdout")))?;
    }
    Ok(0)
}

fn write_markdown(items: &[ListItem], warnings: &[String]) -> AppResult<()> {
    let mut output = std::io::BufWriter::new(std::io::stdout().lock());
    for severity in [Severity::Blocker, Severity::Major, Severity::Minor] {
        let matching: Vec<_> = items
            .iter()
            .filter(|item| item.cut.severity == severity)
            .collect();
        if matching.is_empty() {
            continue;
        }
        writeln!(
            output,
            "## {}",
            match severity {
                Severity::Blocker => "Blocker",
                Severity::Major => "Major",
                Severity::Minor => "Minor",
            }
        )
        .map_err(|error| AppError::from_io(error, std::path::Path::new("stdout")))?;
        for item in matching {
            let id = if item.status == ItemStatus::Resolved {
                format!("~~{}~~", item.cut.id)
            } else {
                item.cut.id.clone()
            };
            let tags = if item.cut.tags.is_empty() {
                String::new()
            } else {
                format!(" ({})", item.cut.tags.join(","))
            };
            writeln!(
                output,
                "- [{id}] {} — {}, {}{tags}",
                item.cut.text, item.cut.agent, item.cut.ts
            )
            .map_err(|error| AppError::from_io(error, std::path::Path::new("stdout")))?;
        }
    }
    for warning in warnings {
        writeln!(output, "> note: {warning}")
            .map_err(|error| AppError::from_io(error, std::path::Path::new("stdout")))?;
    }
    Ok(())
}
