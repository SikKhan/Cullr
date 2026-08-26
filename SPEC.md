# Cullr — Technical Spec (Revision B)

Photo Mechanic-style culling tool for RAW photographs. Linux-first, cross-platform by construction (pure Rust). Open source, GPL-3.0-or-later.

## 1. Goal

Photo Mechanic's ingest/cull loop only: open folder → browse embedded JPEG previews instantly → preview large → mark colors → filter by colors. Keyboard-first workflow.

**Non-goals (v1):** RAW development/demosaic, XMP sidecar writing, catalog/library management, star ratings, compare view.

## 2. Locked decisions

| Decision | Value |
|---|---|
| UI | egui/eframe 0.36 (immediate mode, GPU-rendered) |
| RAW engine | rawler 0.7.x pinned exact; isolated behind `extract.rs`; fallback chain preview → full → thumbnail |
| Storage | rusqlite (bundled) index DB + plain JPEG files in XDG cache dir |
| Parallelism | rayon ingest pool + crossbeam channels streaming into UI |
| Platform | Linux-first (`~/.cache/cullr`, X11 + Wayland verified), portable anywhere |
| License | Cullr: **GPL-3.0-or-later**, public repo from commit one. Dependency: rawler is LGPL-2.1-only — compatible via LGPL §6 (source-available relink), see §12 |
| Engineering standard | rust-best-practices: thiserror in core / anyhow in ui binary only, zero unwrap/expect outside tests, `[workspace.lints]` deny-all, doc tests on core facade |

## 3. Architecture

```
cullr/
├── Cargo.toml                  # workspace + [workspace.lints]
└── crates/
    ├── cullr-core/
    │   └── src/
    │       ├── lib.rs          # public facade: Engine, Events, commands
    │       ├── model.rs        # PhotoId(u64, Copy), PhotoMeta, Label(Copy)
    │       ├── scanner.rs      # walkdir + extension filter + diff vs DB
    │       ├── db.rs           # rusqlite repos, migrations via user_version
    │       ├── extract.rs      # ONLY place touching rawler (dyn boundary)
    │       ├── ingest.rs       # job queue, rayon driver, generation-cancel
    │       ├── cache.rs        # paths, atomic writes (tmp+rename), GC
    │       └── exif.rs         # display-string formatting from RawMetadata
    └── cullr-ui/
        └── src/
            ├── main.rs         # eframe entry, theme, Inter font
            ├── app.rs          # App state machine, event pump
            ├── views/
            │   ├── home.rs     # folder picker + recents
            │   ├── grid.rs     # virtualized contact sheet
            │   ├── loupe.rs    # full preview
            │   └── widgets.rs  # color chip, cell, filter bar, EXIF overlay
            ├── tex.rs          # LRU GPU texture cache + decode workers
            └── input.rs        # shortcut map
```

**Data flow:** UI action → Engine command → core (scan/DB/rayon) → crossbeam channel → App drains events every frame → repaint.

**Threading rules:** GPU texture upload and all egui calls on UI thread only. Ingest = one rayon pool (`num_cpus` capped at 12). Thumb decode for display = small second pool feeding `tex.rs`. SQLite behind a single `Mutex<Connection>`, WAL mode; writes are tiny upserts.

## 4. Data model

Single global DB: `~/.cache/cullr/index.db`. Identity = `(root, rel_path)` guarded by `mtime`+`size`; stale rows re-ingested, never trusted.

```sql
PRAGMA journal_mode=WAL;
CREATE TABLE photos (
  id          INTEGER PRIMARY KEY,
  root        TEXT NOT NULL,
  rel_path    TEXT NOT NULL,
  mtime       INTEGER NOT NULL,
  size        INTEGER NOT NULL,
  width INTEGER, height INTEGER,
  orientation INTEGER NOT NULL DEFAULT 1,     -- EXIF flag 1..8
  rot_cw INTEGER NOT NULL DEFAULT 0,          -- user quarter-turns CW 0..3
  camera TEXT, lens TEXT,
  taken_at TEXT,
  shutter TEXT, aperture REAL, iso INTEGER, focal_mm REAL,
  label       INTEGER NOT NULL DEFAULT 0,   -- 0=none 1..5=R,Y,G,B,P
  status      INTEGER NOT NULL DEFAULT 0,   -- 0=pending 1=ok 2=error 3=missing
  err_msg TEXT,
  preview_path TEXT, thumb_path TEXT,
  jpeg_rel_path TEXT,                        -- companion JPEG of a RAW+JPEG pair
  ingested_at INTEGER,
  UNIQUE(root, rel_path));
CREATE INDEX idx_photos_root  ON photos(root);
CREATE INDEX idx_photos_label ON photos(root, label);
CREATE TABLE roots (id INTEGER PRIMARY KEY, path TEXT UNIQUE NOT NULL, last_opened INTEGER NOT NULL);
CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT);
```

**Cache layout:** `~/.cache/cullr/previews/<hash>.jpg` (re-encoded q88) · `thumbs/<hash>.jpg` (512 px long edge, q85). `<hash>` = md5(root + rel_path + mtime + size). All writes temp-file + rename.

## 5. Pipelines

### 5.1 Scan (target < 300 ms @ 10k files)
walkdir (depth 1 default, recursive opt-in) → extension filter = hardcoded list ∩ `rawler::decoders::supported_extensions()` → stat → upsert rows where changed/new → return ordered `Vec<PhotoMeta>` immediately. Grid shows placeholders right away.

RAW+JPEG pairing: a `.jpg`/`.jpeg` sibling with the same stem (case-insensitive, same directory) attaches to the RAW's row (`jpeg_rel_path`) instead of becoming a photo. The sheet shows one copy — the RAW — with a `RAW+JPEG` tag under its filename; export copies both originals. Unpaired JPEGs are ignored; a JPEG appearing/vanishing next to an untouched RAW updates the tag without re-extraction.

### 5.2 Ingest (per pending file, rayon parallel)
1. Open → `rawler::get_decoder`
2. `raw_metadata()` → orientation, camera/lens, EXIF exposure data (header parse only)
3. Preview acquisition fallback chain: `preview_image()` → `full_image()` → `thumbnail_image()` (first `Some(DynamicImage)` wins)
4. Write `previews/<hash>.jpg` + downscaled `thumbs/<hash>.jpg`
5. Row → `status=ok`, emit `Event::Ingested(PhotoId)`
6. Any failure → `status=2` + `err_msg`, error event, pipeline continues. `catch_unwind` around rawler calls.

Cancellation: generation counter bumped on folder switch; workers check between steps and drop results. Visible-window priority ping reorders queue so near-viewport items jump ahead.

### 5.3 Display textures
`tex.rs`: LRU keyed `(PhotoId, SizeClass{Thumb, Screen})`, byte budget 512 MB. Miss → decode cached JPEG off-thread, rotate upright (EXIF orientation + user `rot_cw` turns) → RGBA8 via channel → `load_texture` next frame on UI thread. Uploads amortized ≤ 6 new textures/frame. Loupe prefetches id±3 neighbors. A rotation change invalidates the resident slot like a new asset path does.

Cached assets keep the sensor orientation they were embedded with; turning them upright is a presentation concern applied once per decode on the worker thread.

## 6. UI spec

Views: Home → Grid ⇄ Loupe. Top bar persists in Grid/Loupe.

**Grid:** virtualized — render only visible rows (+1 margin) from ScrollArea offset. Cell = image + border (accent = cursor) + bottom strip (label dot, truncated filename). Zoom slider + Ctrl+wheel, 128–1024 px cells. Sort: filename / taken_at.

**Loupe:** fit-to-window; wheel zooms toward the cursor between fit and 400% (100% = pixel parity of the preview; `Space`, `Ctrl+1` and double-click toggle fit ↔ 100%); drag pans; arrows navigate within filtered order; Esc → Grid. Photoshop-style zoom aids: `Ctrl+0` fit, `Ctrl+1` 100%, `Ctrl+=`/`Ctrl+-` multiplicative steps, double-click flips the extremes under the cursor, Shift-drag marquee zooms a region to the viewport. Bottom-left pill: zoom % of native resolution with −/+ click steps and scrubby drag. Bottom EXIF bar: `camera · lens · f/xx · 1/xxx s · ISO x · xxmm · timestamp`. Overlay top-right: label + `142 / 3 210`.

**Lightbox:** `L` (grid or loupe) strips all chrome — photo alone on near-black, `W` flips the backdrop to pure white for high-key frames. Zooming, panning, navigation, labels and rotation keep working; `Esc`/`Enter`/`L` restores the loupe chrome before a second press returns to Grid.

**Filter bar:** multi-toggle chips `[All] [○ unlabeled] [R][Y][G][B][P]`, per-chip counts, live refilter. Status bar: shown/total, ingest progress + rate, clickable error count.

**Keyboard:**

| Key | Action |
|---|---|
| `←→↑↓` | move cursor |
| `1..5` / `0` | label red/yellow/green/blue/purple / clear |
| `[` / `]` | rotate 90° left / right (selection or cursor; loupe: current photo) — persisted |
| `Tab` | toggle auto-advance-after-label (persisted) |
| `Enter` / `Esc` | loupe ⇄ grid |
| `Space` | loupe: fit/100% · grid: enter loupe |
| `Ctrl+0` / `Ctrl+1` | loupe: fit ↔ 100% pixel parity |
| `Ctrl+=` / `Ctrl+-` | loupe: zoom in / out a step |
| `L` | lightbox on/off · grid: open straight into it |
| `W` | lightbox: white ↔ black backdrop |
| `Ctrl+A` / `Shift+A` | select all / none |
| `Ctrl+E` | export originals: selection, else the filtered view (bottom-right button); RAW+JPEG pairs copy both files |
| `F` | cycle filter preset: All → Labeled → Unlabeled |
| `?` | shortcut overlay |

**Selection:** cursor + selection set. Click = cursor+select; Ctrl-click toggle; Shift-click range; drag marquee. Digits apply to selection (cursor if empty). Labels persist instantly (single UPDATE).

**Theme:** bg `#16171A`, panels `#1E2023`, text `#D7D9DC` (Inter 13), accent `#E8A33D`. Labels: red `#E5484D`, yellow `#F5C518`, green `#46A758`, blue `#3E63DD`, purple `#9D5CE8`.

## 7. Failure policy

| Failure | Behavior |
|---|---|
| Unsupported/corrupt RAW | gray tile + ⚠ tooltip err_msg; still labelable |
| Preview chain exhausted | upscale thumbnail_image to 512; else error tile |
| File deleted mid-session | row → `missing`, tile dims, removed from nav |
| DB locked/corrupt | toast + read-only degraded mode |
| Worker panic | caught → error tile; pipeline continues |

## 8. Performance budgets (acceptance gates)

| Metric | Budget |
|---|---|
| Scan 10k files | < 300 ms |
| First thumb (cold) | < 1.5 s |
| Warm reopen | visible thumbs < 2 s |
| Ingest throughput | ≥ 20 files/s (8-core NVMe, CR3) |
| Loupe open warm / cold | < 30 ms / < 400 ms |
| Frame time scrolling steady-state | ≤ 16 ms (no stall > 50 ms) |
| GPU texture budget | 512 MB; RSS ≤ 800 MB @ 10k |

All benchmarks run `--release`.

## 9. Engineering standard (rust-best-practices)

- Errors: `thiserror` enums in cullr-core (`ScanError`, `DbError`, `ExtractError` behind `CoreError`); `anyhow` only in cullr-ui main. Zero `unwrap()`/`expect()` outside tests.
- Lints: shared `[workspace.lints]`: deny `clippy::all`, plus `unwrap_used`, perf lints, `redundant_clone`, `needless_collect`. Overrides via `#[expect]` with justification, never bare `#[allow]`.
- Ownership: hot paths clone-audited; `&str`/`&[T]` params; `Copy` derives for PhotoId/Label.
- Dispatch: static internally; `dyn` only at rawler decoder boundary (boxed once in extract.rs) and event sink.
- Tests: descriptive names (`scan_should_skip_hidden_directories`), one assertion per test, doc tests on public core facade, `tempfile` DB tests, fixture integration gated on `CULLR_FIXTURES` env (skip silently when unset).
- Docs: `//` = why, `///` = public API; `#![deny(missing_docs)]` on cullr-core lib. TODOs require issue refs.
- Every task ends with: `cargo fmt --all` + `cargo clippy --workspace --all-targets -- -D warnings` clean.

## 10. Task breakdown

Strict deps within phase, soft across phases.

### Phase A — Skeleton & index
- **T1 Workspace & shell** — git repo, LICENSE (GPLv3), workspace toml + `[workspace.lints]`, both crates, eframe window (dark theme, Inter), tracing init, CI skeleton (GitHub Actions ubuntu: fmt + clippy -D warnings + test), AGENTS.md kept accurate. *Done: app runs, themed empty window.*
- **T2 Scanner** — model.rs, scanner.rs + unit tests. *Done: ordered entries for fixture tree.*
- **T3 DB layer** — db.rs migrations + photos/roots/kv repos, scan-diff upsert, tempfile tests. *Done: double-open → zero redundant rows.*
- **T4 Home view & wiring** — rfd pick, recents, Home→Grid placeholders + counts. *Done: correct file count.*

### Phase B — Extraction engine
- **T5 Extract module** — extract.rs (metadata + fallback chain), cache.rs atomic writes; integration tests gated on `CULLR_FIXTURES`. *Done: CR3/NEF/ARW fixtures produce preview+thumb+EXIF.*
- **T6 Ingest pipeline** — queue, rayon driver, generation-cancel, progress events, panic isolation. *Done: 500-file folder ingested; mid-run switch cancels cleanly.*
- **T7 Grid v1** — virtualized scroll, aspect-fit cells, progressive fill, spinners, error tiles. *Done: §8 scan + first-thumb budgets met.*

### Phase C — Loupe
- **T8 Texture manager** — LRU, decode workers, amortized upload, neighbor prefetch. *Done: 10k-row scroll without stalls.*
- **T9 Loupe view** — fit/zoom/pan, nav keys, EXIF bar, shimmer, position indicator. *Done: loupe budget met.*

### Phase D — Culling workflow
- **T10 Labels** — enum, digit keys, swatches, auto-advance toggle, instant persist. *Done: full keyboard cull pass.*
- **T11 Filters** — chips + counts, live refilter, F presets, status stats. *Done: refilter < 1 frame @ 10k.*
- **T12 Selection** — click/ctrl/shift/marquee, batch labeling. *Done: 100-photo batch relabel, one keystroke.*

### Phase E — Polish & publish
- **T13 Perf hardening** — 50k soak, eviction/backpressure tuning, warm restart path. *Done: §8 fully green.*
- **T14 UX finish** — zoom slider/Ctrl-wheel, sort, `?` overlay, high-DPI (X11/Wayland), remember window/folder, About dialog w/ notices. *Done: real 1 000+ shot card culled end-to-end.*
- **T15 Publish** — README (screenshots, build, third-party notices), release tag + tarball.

### Deferred past v1
Export/copy kept+rejected sets, rename templates, XMP sidecars, recursive-scan UI toggle, compare view.

## 11. Risks

| Risk | Mitigation |
|---|---|
| rawler LGPL-2.1-only composition with GPL app | resolved: open source + source availability satisfies LGPL §6 relink duty; third-party notice shipped |
| rawler no SemVer | pin `=0.7.x`; all usage confined to extract.rs |
| Per-vendor preview quirks (some Fuji RAF lack large previews) | fallback chain §5.2; error tiles never block |
| egui texture-upload jank | amortized uploads + LRU budget (T8) |

## 12. License compliance checklist

- [x] Cullr license expression: `GPL-3.0-or-later`
- [x] LICENSE file with GPLv3 text (T1)
- [x] Cargo.toml `license` field set (T1)
- [x] Third-party notice: rawler © dnglab contributors, LGPL-2.1 (README + About dialog, T14/T15)
- [x] Source-available distribution satisfies LGPL-2.1 §6 relink requirement
