# AGENTS.md — paperclips

Machine-facing contract for agents working in this repo.

## What this is

`paperclips` is a Rust workspace with two binaries:
- **papercuts** — log friction (what to fix) — `.papercuts.jsonl`
- **paperclip** — log wins (what to keep) — `.paperclips.jsonl`

Forked from [treygoff24/papercuts](https://github.com/treygoff24/papercuts) (MIT).

## Build and gate

```bash
cargo build --workspace
cargo test -p papercuts --test cli
```

## Key concepts

- **Cuts** (papercuts): friction. Log when something wasted time or behaved unexpectedly.
- **Clips** (paperclip): wins. Log when something exceeded expectations or saved real time. "Worked correctly" is noise — clips should name the concrete value.
- **Overlaps**: same `--where` in both logs = load-bearing AND sharp-edged
- **`--where`**: component or path the record is about. The join key.

## What to log, when to log, what review produces

| Direction | What to log | When | Format |
|-----------|-------------|------|--------|
| Cut | Friction that wasted time, broke expectations, or caused rework | Immediately, while context is hot | `papercuts add "..." --where <component> --severity <minor/moderate/major> --tag <area>` |
| Clip | Wins that exceeded expectations, saved significant time, or revealed hidden functionality | Same session, before context decays | `paperclip add "..." --where <component> --impact <trivial/solid/huge> --tag <area>` |

`paperclip review` joins both logs on `--where` and produces Overlaps — pairs where the same component both cut you and delivered value. These are the highest-signal records: they tell you exactly where to invest fix-while-keeping effort.

Review reads each overlap pair and assigns a shape:
- **Shape A**: Same component, different aspect — fix the edge, keep the value
- **Shape B**: Value confirmed, access broken — amplify discoverability
- **Shape C**: Context-dependence — add a mode or config flag

## Canonical example (Shape A)

The `bridge-trigger` component in this repo's `.paperclips.jsonl` demonstrates a clean Shape A overlap:

```
CUT:  "fires without priority check" — sharp edge
CLIP: "auto-dispatches useful work, cleared the backlog unattended" — value delivered
→ Shape A: the trigger works (autonomous dispatch, cleared backlog) but has a sharp edge (no priority check)
```

## Invariants

- Append-only. stdout = data only. stderr = errors only.
- Old logs without `where` still work (review treats them as unjoinable).
- The `.paperclips.jsonl` in this repo is dogfood — 3 entries (2 clips, 1 cut) demonstrating production format.

## List output

Both list commands default to at most 50 matching records.
Use `--limit N` to change that cap.
When the cap omits matches, Markdown appends a `> note:` with the shown and total counts and a limit that displays all matches.
JSON carries the same notice in `meta.warnings`; `data.count` counts returned items, `data.total` counts all matches after filtering, and `data.truncated` reports whether any matches were omitted.

`paperclip list --format md` groups clips by impact and renders one line per clip with its id, text, impact, optional `where`, and any non-open status.
A missing clips log produces an empty Markdown listing with exit code 0.
