# paperclips

Two-directional friction log for AI agents: record what breaks, record what works, then review both together.

## Why

AI agents generate a lot of signal during work — dead-end tool calls, broken links, footgun configs on one side; helpers that saved time, docs that answered exactly right on the other. Most friction-logging tools only capture the negative. `paperclips` captures both directions and joins them so you can see where something is simultaneously load-bearing and sharp-edged.

- **papercuts** — log friction (what to fix) — `.papercuts.jsonl`
- **paperclip** — log wins (what to keep) — `.paperclips.jsonl`

Without `--file` or the `PAPERCUTS_FILE`/`PAPERCLIP_FILE` overrides, each log lives at the repository root.
From a linked Git worktree, both binaries instead use the main checkout's `.scratch/papercuts.jsonl` and `.scratch/paperclips.jsonl`, adding `.scratch/` to the repository's `.git/info/exclude` on the first `add` when nothing ignores it, so entries outlive the worktree and never enter its branch.

## Lifecycle

The lifecycle inverts, not just the sentiment.

**Cuts** end in `resolve` — the problem is gone. Someone fixed the friction, and the record closes. The goal is elimination.

**Clips** end in `promote` — the win gets written into CLAUDE.md, AGENTS.md, or a skill so future sessions inherit it. The goal is compounding. A clip that stays in the log is a single observation. A clip promoted to docs is a permanent capability upgrade.

```
Cuts:   add → [someone fixes it] → resolve    (friction eliminated)
Clips:  add → [someone codifies it] → promote  (win inherited)
```

When both logs share a `--where` on the same component, `paperclip review` surfaces it as an **Overlap**: the highest-information record in the system.

## Install

```bash
cargo install --git https://github.com/dlorp/paperclips --bins
```

## Minimum Supported Rust Version

**Rust 1.89** — enforced via `rust-version` in each crate's `Cargo.toml`.

## Quick start

```bash
# Log friction
papercuts add "cache invalidation races on concurrent writes" \
  --tag storage --severity major --where kv-store

# Log wins
paperclip add "bulk insert API cut sync time from minutes to seconds" \
  --tag storage --impact huge --where kv-store

# Review both
paperclip review --format md
```

## Overlaps

`review` joins cuts × clips on the `--where` field (exact > prefix > tag) and renders:

```
Overlap shapes — read each pair and assign one:
  A. Same component, different aspect — fix the edge, keep the value
  B. Value confirmed, access broken — amplify discoverability
  C. Context-dependence — add a mode or config flag

### bridge-trigger  [exact · 1 cut / 1 clip]
  CUT  [major] pc_a1b2 — fires without priority check
  CLIP [solid] cl_f6e5 — auto-dispatches useful work, cleared the backlog unattended
  → Shape A: the trigger works (cleared backlog), but the edge case is sharp (no priority check)
  Disposition: add a gate before dispatch ___________
```

## Review output format

`--format md` (default): Markdown with overlap shapes ready for human disposition.

`--format json`: Machine-readable join records for downstream processing.

## Dogfooding

This repo eats its own cooking. `.paperclips.jsonl` and `.papercuts.jsonl` are committed with real entries — the overlap report in this README is a copy-paste from the repo's own log, not a hypothetical.

## Attribution

Forked from [treygoff24/papercuts](https://github.com/treygoff24/papercuts) (MIT). The `papercuts` binary is behavior-compatible with upstream.

## License

MIT
