# AGENTS.md

Cullr: a Photo Mechanic-style photo culling tool for RAW files (Linux-first, cross-platform via Rust). Open source, GPL-3.0-or-later. Full spec: `SPEC.md`.

## Stack (decided)

- Pure Rust. UI: **egui/eframe** (immediate mode, GPU-rendered). Do not introduce web views or alternative GUI frameworks.
- RAW handling: **rawler** for parsing and extracting embedded JPEG previews. Never decode full RAW sensor data — culling speed depends entirely on using embedded JPEGs.
- RAW engine is **rawler** (LGPL-2.1-only) — pin exact version, confine all usage to `core/src/extract.rs`.
- Storage: **rusqlite** (bundled feature) for the index DB; extracted previews and resized thumbnails as plain JPEG files under `~/.cache/cullr/` (`dirs::cache_dir()`, XDG).
- Parallelism: `rayon` for extraction/thumb jobs; thumbnails stream to the UI via channels (progressive grid fill).

## Layout

Cargo workspace:

- `crates/cullr-core/` — scanning, preview extraction, thumbnailing, SQLite index, color-label store. Must stay GUI-free.
- `crates/cullr-ui/` — the eframe application.

(Planned structure as of project start; keep core/UI separation when adding code.)

## Key invariants

- All extracted assets are cached and keyed by path + mtime; re-opening a known folder must skip re-extraction.
- The contact sheet is virtualized: only visible cells own GPU textures (LRU texture cache). Don't load textures for offscreen items.
- Extraction failures must degrade gracefully (error tile), never panic or block the pipeline.
- v1 scope is culling only: browse, preview, color labels, filter by labels. No RAW editing; XMP sidecar writing is explicitly out of scope for v1.

## Commands

- Run app: `cargo run -p cullr-ui`
- Tests: `cargo test --workspace`
- Lint: `cargo clippy --workspace -- -D warnings`
- Format: `cargo fmt --all`
