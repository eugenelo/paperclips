use crate::error::{AppError, AppResult};
use crate::{
    ClipListItem, ClipNote, ClipRecord, ClipStatus, CutRecord, ItemStatus, ListItem, NoteRecord,
    PromoteRecord, Resolution, ResolveRecord,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::Duration;

const LOCK_ATTEMPTS: usize = 50;
const LOCK_DELAY: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct ResolvedFile {
    pub path: PathBuf,
    pub explicit: bool,
    pub repo: Option<PathBuf>,
    /// Main checkout whose private `.scratch/` log a linked worktree's default discovery chose.
    pub main_checkout: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub struct FoldResult {
    pub items: Vec<ListItem>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Default)]
pub struct ClipFoldResult {
    pub items: Vec<ClipListItem>,
    pub warnings: Vec<String>,
}

#[derive(Default)]
struct WarningCounts {
    torn: usize,
    malformed: usize,
    unknown: usize,
    duplicate_cuts: usize,
    duplicate_resolves: usize,
    orphans: usize,
}

#[derive(Default)]
struct ClipWarningCounts {
    torn: usize,
    malformed: usize,
    unknown: usize,
    duplicate_clips: usize,
    duplicate_promotes: usize,
    orphan_promotes: usize,
    orphan_notes: usize,
}

/// Resolve the papercuts log: `--file`, then `PAPERCUTS_FILE`, then the repository default.
/// A linked worktree shares `<main checkout>/.scratch/papercuts.jsonl`, so entries outlive
/// the worktree and never land in its branch; other repositories use `<root>/.papercuts.jsonl`.
/// @param flag Optional `--file` path.
/// @return The resolved log location.
/// @throws AppError When the working directory, worktree metadata, or home directory is unavailable.
pub fn discover(flag: Option<PathBuf>) -> AppResult<ResolvedFile> {
    let cwd = std::env::current_dir().map_err(|error| AppError::from_io(error, Path::new(".")))?;
    let repo = find_repo_root(&cwd);
    if let Some(path) = flag {
        return Ok(ResolvedFile {
            path: absolute(&cwd, path),
            explicit: true,
            repo,
            main_checkout: None,
        });
    }
    if let Some(path) = std::env::var_os("PAPERCUTS_FILE")
        && !path.is_empty()
    {
        return Ok(ResolvedFile {
            path: absolute(&cwd, PathBuf::from(path)),
            explicit: true,
            repo,
            main_checkout: None,
        });
    }
    if let Some(root) = repo.clone() {
        return repository_default(root, "papercuts.jsonl", ".papercuts.jsonl");
    }
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            AppError::config(
                "cannot resolve the home directory for the default papercuts file",
                "Set HOME or pass --file PATH.",
            )
        })?;
    Ok(ResolvedFile {
        path: absolute(&cwd, home).join(".papercuts/log.jsonl"),
        explicit: false,
        repo: None,
        main_checkout: None,
    })
}

/// Resolve the paperclips log: `--file`, then `PAPERCLIP_FILE`, then the repository default.
/// A linked worktree shares `<main checkout>/.scratch/paperclips.jsonl`, so entries outlive
/// the worktree and never land in its branch; other repositories use `<root>/.paperclips.jsonl`.
/// @param flag Optional `--file` path.
/// @return The resolved log location.
/// @throws AppError When the working directory, worktree metadata, or home directory is unavailable.
pub fn discover_clips(flag: Option<PathBuf>) -> AppResult<ResolvedFile> {
    let cwd = std::env::current_dir().map_err(|error| AppError::from_io(error, Path::new(".")))?;
    let repo = find_repo_root(&cwd);
    if let Some(path) = flag {
        return Ok(ResolvedFile {
            path: absolute(&cwd, path),
            explicit: true,
            repo,
            main_checkout: None,
        });
    }
    if let Some(path) = std::env::var_os("PAPERCLIP_FILE")
        && !path.is_empty()
    {
        return Ok(ResolvedFile {
            path: absolute(&cwd, PathBuf::from(path)),
            explicit: true,
            repo,
            main_checkout: None,
        });
    }
    if let Some(root) = repo.clone() {
        return repository_default(root, "paperclips.jsonl", ".paperclips.jsonl");
    }
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            AppError::config(
                "cannot resolve the home directory for the default paperclips file",
                "Set HOME or pass --file PATH.",
            )
        })?;
    Ok(ResolvedFile {
        path: absolute(&cwd, home).join(".paperclips/log.jsonl"),
        explicit: false,
        repo: None,
        main_checkout: None,
    })
}

/// Choose a repository's default log: the main checkout's private log for a linked worktree.
/// @param root Repository root found by `find_repo_root`.
/// @param shared File name under `<main checkout>/.scratch/` for a linked worktree.
/// @param local File name under `root` for any other repository.
/// @return The resolved default log; `repo` stays the worktree root for record paths.
/// @throws AppError When a linked worktree's Git metadata cannot be read.
fn repository_default(root: PathBuf, shared: &str, local: &str) -> AppResult<ResolvedFile> {
    if let Some(main) = linked_main_checkout(&root)? {
        return Ok(ResolvedFile {
            path: main.join(".scratch").join(shared),
            explicit: false,
            repo: Some(root),
            main_checkout: Some(main),
        });
    }
    Ok(ResolvedFile {
        path: root.join(local),
        explicit: false,
        repo: Some(root),
        main_checkout: None,
    })
}

/// Find the main checkout of a linked worktree from its Git metadata, without running Git.
/// A linked worktree's `.git` file names a per-worktree directory holding `commondir`; a
/// submodule's does not. A bare repository's worktrees have no main checkout.
/// @param root Repository root found by `find_repo_root`.
/// @return The canonical main checkout, or None when `root` is not a linked worktree of one.
/// @throws AppError When `.git`, `commondir`, or the common directory cannot be read.
pub fn linked_main_checkout(root: &Path) -> AppResult<Option<PathBuf>> {
    let dot_git = root.join(".git");
    if !dot_git.is_file() {
        return Ok(None);
    }
    let content =
        std::fs::read_to_string(&dot_git).map_err(|error| AppError::from_io(error, &dot_git))?;
    let Some(gitdir) = content.strip_prefix("gitdir:").map(str::trim) else {
        return Ok(None);
    };
    let gitdir = root.join(gitdir);
    let commondir = gitdir.join("commondir");
    if !commondir.is_file() {
        return Ok(None);
    }
    let common = std::fs::read_to_string(&commondir)
        .map_err(|error| AppError::from_io(error, &commondir))?;
    let common = gitdir.join(common.trim());
    let common = common
        .canonicalize()
        .map_err(|error| AppError::from_io(error, &common))?;
    let Some(main) = common.parent() else {
        return Ok(None);
    };
    let owned = main
        .join(".git")
        .canonicalize()
        .is_ok_and(|candidate| candidate == common);
    Ok(owned.then(|| main.to_path_buf()))
}

/// Keep a main checkout's shared `.scratch/` log out of Git before the first append,
/// adding `.scratch/` to the repository's `info/exclude` when nothing ignores it yet.
/// @param main Main checkout holding the shared log.
/// @param log Shared log path below `<main>/.scratch/`.
/// @return A warning naming the exclude file when this call added the rule, else None.
/// @throws AppError When Git cannot answer, or the rule cannot be written or does not take effect.
pub fn ignore_scratch(main: &Path, log: &Path) -> AppResult<Option<String>> {
    let ignored = || -> AppResult<bool> {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(main)
            .args(["check-ignore", "-q", "--"])
            .arg(log)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|error| AppError::from_io(error, Path::new("git")))?;
        match status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(AppError::config(
                format!("git check-ignore failed for {}", log.display()),
                "Repair the main checkout's Git state, or pass --file PATH.",
            )),
        }
    };
    if ignored()? {
        return Ok(None);
    }
    let exclude = main.join(".git/info/exclude");
    if let Some(parent) = exclude.parent() {
        std::fs::create_dir_all(parent).map_err(|error| AppError::from_io(error, parent))?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(&exclude)
        .map_err(|error| AppError::from_io(error, &exclude))?;
    let length = file
        .seek(SeekFrom::End(0))
        .map_err(|error| AppError::from_io(error, &exclude))?;
    let mut last = [0u8; 1];
    if length > 0 {
        file.seek(SeekFrom::Start(length - 1))
            .and_then(|_| file.read_exact(&mut last))
            .map_err(|error| AppError::from_io(error, &exclude))?;
    }
    let rule = if length > 0 && last[0] != b'\n' { "\n.scratch/\n" } else { ".scratch/\n" };
    file.write_all(rule.as_bytes())
        .map_err(|error| AppError::from_io(error, &exclude))?;
    if !ignored()? {
        return Err(AppError::config(
            format!("{} still does not ignore .scratch/", main.display()),
            format!("Check {}, or pass --file PATH.", exclude.display()),
        ));
    }
    Ok(Some(format!(
        "added .scratch/ to {} so the shared log stays uncommitted",
        exclude.display()
    )))
}

/// Convert an absolute path to a repo-relative path.
/// Returns "." when path equals the repo root. Falls back to absolute when
/// no repo root is known or the path is outside the repo.
pub fn repo_relative(repo: Option<&Path>, path: &Path) -> String {
    match repo {
        Some(root) => match path.strip_prefix(root) {
            Ok(rel) => {
                let s = rel.to_string_lossy();
                if s.is_empty() {
                    ".".to_string()
                } else {
                    s.into_owned()
                }
            }
            Err(_) => path.to_string_lossy().into_owned(),
        },
        None => path.to_string_lossy().into_owned(),
    }
}

pub fn find_repo_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|candidate| {
            let dot_git = candidate.join(".git");
            if !dot_git.exists() {
                return false;
            }
            if dot_git.is_dir() {
                dot_git.join("HEAD").exists()
            } else if dot_git.is_file() {
                std::fs::read_to_string(&dot_git)
                    .is_ok_and(|content| content.starts_with("gitdir:"))
            } else {
                false
            }
        })
        .map(Path::to_path_buf)
}

fn absolute(cwd: &Path, path: PathBuf) -> PathBuf {
    let joined = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

pub fn with_shared<T>(path: &Path, action: impl FnOnce(&mut File) -> AppResult<T>) -> AppResult<T> {
    let mut file = File::open(path).map_err(|error| AppError::from_log_open(error, path))?;
    lock(&file, path, false)?;
    let result = action(&mut file);
    let unlock = file
        .unlock()
        .map_err(|error| AppError::from_io(error, path));
    match (result, unlock) {
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        (Ok(value), Ok(())) => Ok(value),
    }
}

pub fn with_exclusive<T>(
    path: &Path,
    create: bool,
    action: impl FnOnce(&mut File) -> AppResult<T>,
) -> AppResult<T> {
    if create && let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| AppError::from_io(error, parent))?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(create)
        .open(path)
        .map_err(|error| AppError::from_log_open(error, path))?;
    lock(&file, path, true)?;
    let result = action(&mut file);
    let unlock = file
        .unlock()
        .map_err(|error| AppError::from_io(error, path));
    match (result, unlock) {
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        (Ok(value), Ok(())) => Ok(value),
    }
}

fn lock(file: &File, path: &Path, exclusive: bool) -> AppResult<()> {
    for attempt in 0..LOCK_ATTEMPTS {
        let result = if exclusive {
            file.try_lock()
        } else {
            file.try_lock_shared()
        };
        match result {
            Ok(()) => return Ok(()),
            Err(error) => {
                let error: std::io::Error = error.into();
                if error.kind() != std::io::ErrorKind::WouldBlock {
                    return Err(AppError::from_io(error, path));
                }
                if attempt + 1 < LOCK_ATTEMPTS {
                    thread::sleep(LOCK_DELAY);
                }
            }
        }
    }
    Err(AppError::lock_timeout(path))
}

pub fn read_bytes(file: &mut File, path: &Path) -> AppResult<Vec<u8>> {
    file.seek(SeekFrom::Start(0))
        .and_then(|_| {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).map(|_| bytes)
        })
        .map_err(|error| AppError::from_io(error, path))
}

pub fn append_json<T: serde::Serialize>(
    file: &mut File,
    path: &Path,
    prior: &[u8],
    record: &T,
) -> AppResult<()> {
    let original_len = file
        .metadata()
        .map_err(|error| AppError::from_io(error, path))?
        .len();
    let mut bytes = Vec::new();
    if !prior.is_empty() && !prior.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    serde_json::to_writer(&mut bytes, record)
        .map_err(|error| AppError::internal(error.to_string()))?;
    bytes.push(b'\n');
    if let Err(error) = file.write_all(&bytes) {
        if let Err(rollback) = file.set_len(original_len) {
            return Err(AppError {
                code: "io_error",
                message: format!(
                    "append failed: {error}; rollback to original length {original_len} failed: {rollback}"
                ),
                details: json!({}),
                retryable: false,
                suggested_fix: "Check the papercuts file and filesystem, then retry.".into(),
                exit_code: 74,
            });
        }
        return Err(AppError::from_io(error, path));
    }
    Ok(())
}

// ── Cut fold ─────────────────────────────────────────────────────────

pub fn fold_bytes(bytes: &[u8]) -> FoldResult {
    let mut cuts = BTreeMap::<String, CutRecord>::new();
    let mut resolves = HashMap::<String, ResolveRecord>::new();
    let mut counts = WarningCounts::default();
    let complete_len = if bytes.is_empty() || bytes.ends_with(b"\n") {
        bytes.len()
    } else {
        counts.torn += 1;
        bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |i| i + 1)
    };

    let complete = &bytes[..complete_len];
    let complete = complete.strip_suffix(b"\n").unwrap_or(complete);
    for raw in complete.split(|byte| *byte == b'\n') {
        if complete.is_empty() {
            break;
        }
        let Ok(value) = serde_json::from_slice::<Value>(raw) else {
            counts.malformed += 1;
            continue;
        };
        match value.get("kind").and_then(Value::as_str) {
            Some("cut") => match serde_json::from_value::<CutRecord>(value) {
                Ok(mut cut) => {
                    if cut.ts.parse::<jiff::Timestamp>().is_err() {
                        counts.malformed += 1;
                        continue;
                    }
                    cut.tags.sort();
                    if cuts.contains_key(&cut.id) {
                        counts.duplicate_cuts += 1;
                    } else {
                        cuts.insert(cut.id.clone(), cut);
                    }
                }
                Err(_) => counts.malformed += 1,
            },
            Some("resolve") => match serde_json::from_value::<ResolveRecord>(value) {
                Ok(resolve) => {
                    if resolve.ts.parse::<jiff::Timestamp>().is_err() {
                        counts.malformed += 1;
                        continue;
                    }
                    if resolves.contains_key(&resolve.id) {
                        counts.duplicate_resolves += 1;
                    } else {
                        resolves.insert(resolve.id.clone(), resolve);
                    }
                }
                Err(_) => counts.malformed += 1,
            },
            _ => counts.unknown += 1,
        }
    }

    for id in resolves.keys() {
        if !cuts.contains_key(id) {
            counts.orphans += 1;
        }
    }
    let mut items: Vec<_> = cuts
        .into_values()
        .map(|cut| {
            let resolution = resolves.get(&cut.id).map(|resolve| Resolution {
                ts: resolve.ts.clone(),
                agent: resolve.agent.clone(),
                note: resolve.note.clone(),
            });
            ListItem {
                status: if resolution.is_some() {
                    ItemStatus::Resolved
                } else {
                    ItemStatus::Open
                },
                cut,
                resolution,
            }
        })
        .collect();
    items.sort_by(|left, right| {
        let left_ts = left.cut.ts.parse::<jiff::Timestamp>().ok();
        let right_ts = right.cut.ts.parse::<jiff::Timestamp>().ok();
        right
            .cut
            .severity
            .rank()
            .cmp(&left.cut.severity.rank())
            .then_with(|| right_ts.cmp(&left_ts))
            .then_with(|| left.cut.id.cmp(&right.cut.id))
    });

    let mut warnings = Vec::new();
    cut_warning(&mut warnings, counts.torn, "torn final line");
    cut_warning(&mut warnings, counts.malformed, "malformed line");
    cut_warning(&mut warnings, counts.unknown, "unknown event");
    cut_warning(&mut warnings, counts.duplicate_cuts, "duplicate cut");
    cut_warning(
        &mut warnings,
        counts.duplicate_resolves,
        "duplicate resolve",
    );
    cut_warning(&mut warnings, counts.orphans, "orphan resolve");
    FoldResult { items, warnings }
}

fn cut_warning(warnings: &mut Vec<String>, count: usize, label: &str) {
    if count > 0 {
        warnings.push(format!(
            "skipped {count} {label}{}",
            if count == 1 { "" } else { "s" }
        ));
    }
}

// ── Clip fold ────────────────────────────────────────────────────────

pub fn fold_clip_bytes(bytes: &[u8]) -> ClipFoldResult {
    let mut clips = BTreeMap::<String, ClipRecord>::new();
    let mut promotes = HashMap::<String, PromoteRecord>::new();
    let mut notes = HashMap::<String, Vec<NoteRecord>>::new();
    let mut counts = ClipWarningCounts::default();

    let complete_len = if bytes.is_empty() || bytes.ends_with(b"\n") {
        bytes.len()
    } else {
        counts.torn += 1;
        bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |i| i + 1)
    };

    let complete = &bytes[..complete_len];
    let complete = complete.strip_suffix(b"\n").unwrap_or(complete);

    if complete.is_empty() {
        return ClipFoldResult::default();
    }

    for raw in complete.split(|byte| *byte == b'\n') {
        let Ok(value) = serde_json::from_slice::<Value>(raw) else {
            counts.malformed += 1;
            continue;
        };
        match value.get("kind").and_then(Value::as_str) {
            Some("clip") => match serde_json::from_value::<ClipRecord>(value) {
                Ok(mut clip) => {
                    if clip.ts.parse::<jiff::Timestamp>().is_err() {
                        counts.malformed += 1;
                        continue;
                    }
                    clip.tags.sort();
                    if clips.contains_key(&clip.id) {
                        counts.duplicate_clips += 1;
                    } else {
                        clips.insert(clip.id.clone(), clip);
                    }
                }
                Err(_) => counts.malformed += 1,
            },
            Some("promote") => match serde_json::from_value::<PromoteRecord>(value) {
                Ok(promote) => {
                    if promote.ts.parse::<jiff::Timestamp>().is_err() {
                        counts.malformed += 1;
                        continue;
                    }
                    if promotes.contains_key(&promote.id) {
                        counts.duplicate_promotes += 1;
                    } else {
                        promotes.insert(promote.id.clone(), promote);
                    }
                }
                Err(_) => counts.malformed += 1,
            },
            Some("note") => match serde_json::from_value::<NoteRecord>(value) {
                Ok(note) => {
                    if note.ts.parse::<jiff::Timestamp>().is_err() {
                        counts.malformed += 1;
                        continue;
                    }
                    notes.entry(note.id.clone()).or_default().push(note);
                }
                Err(_) => counts.malformed += 1,
            },
            _ => counts.unknown += 1,
        }
    }

    for id in promotes.keys() {
        if !clips.contains_key(id) {
            counts.orphan_promotes += 1;
        }
    }
    for id in notes.keys() {
        if !clips.contains_key(id) {
            counts.orphan_notes += 1;
        }
    }

    let mut items: Vec<_> = clips
        .into_values()
        .map(|clip| {
            let promoted = promotes.contains_key(&clip.id);
            let clip_notes: Vec<ClipNote> = notes
                .get(&clip.id)
                .map(|v| {
                    v.iter()
                        .map(|n| ClipNote {
                            ts: n.ts.clone(),
                            agent: n.agent.clone(),
                            text: n.text.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let status = if promoted {
                ClipStatus::Promoted
            } else if !clip_notes.is_empty() {
                ClipStatus::Noted
            } else {
                ClipStatus::Open
            };
            ClipListItem {
                clip,
                status,
                notes: clip_notes,
            }
        })
        .collect();
    items.sort_by(|left, right| {
        let left_ts = left.clip.ts.parse::<jiff::Timestamp>().ok();
        let right_ts = right.clip.ts.parse::<jiff::Timestamp>().ok();
        right
            .clip
            .impact
            .rank()
            .cmp(&left.clip.impact.rank())
            .then_with(|| right_ts.cmp(&left_ts))
            .then_with(|| left.clip.id.cmp(&right.clip.id))
    });

    let mut warnings = Vec::new();
    clip_warning(&mut warnings, counts.torn, "torn final line");
    clip_warning(&mut warnings, counts.malformed, "malformed line");
    clip_warning(&mut warnings, counts.unknown, "unknown event");
    clip_warning(&mut warnings, counts.duplicate_clips, "duplicate clip");
    clip_warning(
        &mut warnings,
        counts.duplicate_promotes,
        "duplicate promote",
    );
    clip_warning(&mut warnings, counts.orphan_promotes, "orphan promote");
    clip_warning(&mut warnings, counts.orphan_notes, "orphan note");
    ClipFoldResult { items, warnings }
}

fn clip_warning(warnings: &mut Vec<String>, count: usize, label: &str) {
    if count > 0 {
        warnings.push(format!(
            "skipped {count} {label}{}",
            if count == 1 { "" } else { "s" }
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Severity, compute_id};

    fn cut(id: &str) -> String {
        cut_with_text(id, "x")
    }

    fn cut_with_text(id: &str, text: &str) -> String {
        serde_json::json!({
            "kind":"cut", "id":id, "ts":"2026-07-09T00:00:00.000Z",
            "agent":"a", "text":text, "tags":[], "severity":"minor",
            "cwd":"/tmp", "repo":null
        })
        .to_string()
    }

    fn resolve(id: &str) -> String {
        serde_json::json!({
            "kind":"resolve", "id":id, "ts":"2026-07-10T00:00:00.000Z",
            "agent":"a", "note":null
        })
        .to_string()
    }

    #[test]
    fn fold_matrix() {
        let id = compute_id("2026-07-09T00:00:00.000Z", "a", "x", Severity::Minor, &[]);
        let cases = [
            ("cut", format!("{}\n", cut(&id)), 1, ItemStatus::Open, 0),
            (
                "resolve before cut",
                format!("{}\n{}\n", resolve(&id), cut(&id)),
                1,
                ItemStatus::Resolved,
                0,
            ),
            (
                "duplicates",
                format!(
                    "{}\n{}\n{}\n{}\n",
                    cut(&id),
                    cut(&id),
                    resolve(&id),
                    resolve(&id)
                ),
                1,
                ItemStatus::Resolved,
                2,
            ),
            (
                "unknown malformed orphan",
                format!(
                    "{{\"kind\":\"future\"}}\nnope\n{}\n{}\n",
                    resolve("pc_deadbeef0000"),
                    cut(&id)
                ),
                1,
                ItemStatus::Open,
                3,
            ),
            (
                "torn tail",
                format!("{}\n{{\"kind\":", cut(&id)),
                1,
                ItemStatus::Open,
                1,
            ),
            (
                "all adversarial orderings interleaved",
                format!(
                    "{}\n{{\"kind\":\"future\"}}\n{}\n{}\n{}\n{}\n{}\nnope\n{{\"kind\":",
                    resolve(&id),
                    cut(&id),
                    cut(&id),
                    cut_with_text(&id, "conflicting payload"),
                    resolve(&id),
                    resolve("pc_deadbeef0000"),
                ),
                1,
                ItemStatus::Resolved,
                6,
            ),
        ];
        for (name, input, item_count, status, warning_count) in cases {
            let folded = fold_bytes(input.as_bytes());
            assert_eq!(folded.items.len(), item_count, "{name}");
            if !folded.items.is_empty() {
                assert_eq!(folded.items[0].status, status, "{name}");
                assert_eq!(folded.items[0].cut.text, "x", "{name}");
            }
            assert_eq!(folded.warnings.len(), warning_count, "{name}");
        }
    }

    #[test]
    fn fold_clips_basic() {
        let clip = serde_json::json!({
            "kind":"clip", "id":"cl_aabbccdd0011", "ts":"2026-07-09T00:00:00.000Z",
            "agent":"a", "text":"works great", "tags":["tooling"], "impact":"solid",
            "where":"bridge-trigger", "cwd":"/tmp", "repo":null
        });
        let promote = serde_json::json!({
            "kind":"promote", "id":"cl_aabbccdd0011", "ts":"2026-07-10T00:00:00.000Z",
            "agent":"a"
        });
        let note = serde_json::json!({
            "kind":"note", "id":"cl_aabbccdd0011", "ts":"2026-07-10T01:00:00.000Z",
            "agent":"b", "text":"confirmed"
        });

        // open clip
        let input = format!("{}\n", clip);
        let folded = fold_clip_bytes(input.as_bytes());
        assert_eq!(folded.items.len(), 1);
        assert_eq!(folded.items[0].status, ClipStatus::Open);
        assert_eq!(
            folded.items[0].clip.where_loc.as_deref(),
            Some("bridge-trigger")
        );

        // promoted clip
        let input = format!("{}\n{}\n", clip, promote);
        let folded = fold_clip_bytes(input.as_bytes());
        assert_eq!(folded.items.len(), 1);
        assert_eq!(folded.items[0].status, ClipStatus::Promoted);

        // noted clip (note without promote)
        let input = format!("{}\n{}\n", clip, note);
        let folded = fold_clip_bytes(input.as_bytes());
        assert_eq!(folded.items.len(), 1);
        assert_eq!(folded.items[0].status, ClipStatus::Noted);
        assert_eq!(folded.items[0].notes.len(), 1);
        assert_eq!(folded.items[0].notes[0].text, "confirmed");

        // promoted then noted -> promoted (promote takes precedence)
        let input = format!("{}\n{}\n{}\n", clip, promote, note);
        let folded = fold_clip_bytes(input.as_bytes());
        assert_eq!(folded.items.len(), 1);
        assert_eq!(folded.items[0].status, ClipStatus::Promoted);
        assert_eq!(folded.items[0].notes.len(), 1);
    }
}
