//! GPU texture pipeline for grid/loupe imagery (SPEC §5.3).
//!
//! Cached thumbnail JPEGs are decoded on small worker threads and uploaded
//! to the GPU here, on the UI thread, at a capped rate per frame. Live
//! textures form an LRU keyed by photo id with a byte budget, so only
//! recently seen cells own GPU memory (SPEC key invariant) and scrolling a
//! huge folder cannot exhaust VRAM.
//!
//! Stalls under fast scrolling are prevented by three mechanisms:
//!
//! * **Priority queues** — visible cells enqueue on the demand channel,
//!   prefetch neighbours on a second one; workers always drain demand
//!   first, so neighbour loading can never delay what is on screen.
//! * **Epoch cancellation** — every request carries the current focus
//!   epoch. When the visible set changes, the epoch is bumped and workers
//!   skip stale jobs in O(1) instead of chewing through thousands of
//!   decode requests for rows the user already scrolled past.
//! * **Bounded backlog** — at most [`READY_CAP`] decoded images wait in
//!   RAM for an upload slot; overflow drops the oldest and forgets its
//!   pending entry so it can be re-demanded later. RSS stays flat no
//!   matter how fast the sheet streams by.
//!
//! Correctness across re-ingest: slots remember which asset path they were
//! built from. When an id's thumb path changes (file edited → new cache
//! hash), the stale slot is dropped and the asset re-decodes; results for
//! superseded requests are discarded without ever reaching the GPU.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};
use eframe::egui;

use cullr_core::PhotoId;

/// Byte ceiling for live textures (SPEC §8: 512 MB GPU texture budget).
const BYTE_BUDGET: usize = 512 * 1024 * 1024;
/// New GPU uploads per frame (SPEC §5.3: amortized, ≤ 6).
const MAX_UPLOADS_PER_FRAME: usize = 6;
/// Background JPEG decoders feeding [`Textures`]; decode of a ≤512 px JPEG
/// is sub-millisecond, so two workers outpace both ingest and upload caps.
const DECODE_WORKERS: usize = 2;
/// Decoded-but-not-yet-uploaded images kept in RAM (≈70 MB at 512 px
/// thumbs). Beyond this, oldest results are dropped and re-demanded if the
/// user scrolls back, keeping RSS flat during burst scrolls.
const READY_CAP: usize = 96;
/// Prefetch dispatch pauses while this many decodes are outstanding, so
/// neighbour loading can neither starve visible cells nor balloon RSS.
const PREFETCH_PENDING_CAP: usize = 256;

/// Outcome of asking the cache for a cell image.
#[derive(Clone)]
pub enum TextureState {
    /// Decoded and ready to paint.
    Ready(egui::TextureHandle),
    /// Decode queued or in flight; draw a spinner placeholder.
    Loading,
    /// The cached JPEG exists but could not be decoded; draw a fallback
    /// instead of retrying forever.
    Broken,
}

/// Which rendition of a photo a texture serves (SPEC §5.3 keys textures
/// by `(PhotoId, SizeClass)`).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SizeClass {
    /// Contact-sheet thumbnail (≤ 512 px long edge).
    Thumb,
    /// Loupe-sized full preview.
    Screen,
}

/// Cache key: one photo at one size class. Both classes of an id live in
/// the cache independently — a grid thumb says nothing about loupe state.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TexKey {
    /// Photo this texture belongs to.
    pub id: PhotoId,
    /// Rendition requested.
    pub class: SizeClass,
}

impl TexKey {
    /// Key for a contact-sheet thumbnail.
    pub fn thumb(id: PhotoId) -> Self {
        Self {
            id,
            class: SizeClass::Thumb,
        }
    }

    /// Key for a loupe screen preview.
    pub fn screen(id: PhotoId) -> Self {
        Self {
            id,
            class: SizeClass::Screen,
        }
    }

    /// Texture-name prefix so both classes of an id coexist in the GPU
    /// texture registry without colliding.
    fn class_prefix(self) -> &'static str {
        match self.class {
            SizeClass::Thumb => "thumb",
            SizeClass::Screen => "screen",
        }
    }
}

/// Tunables; production values come from [`Textures::new`], tests shrink
/// them to make eviction, upload caps and backlog drops deterministic.
struct Limits {
    byte_budget: usize,
    max_uploads: usize,
    ready_cap: usize,
    prefetch_pending_cap: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            byte_budget: BYTE_BUDGET,
            max_uploads: MAX_UPLOADS_PER_FRAME,
            ready_cap: READY_CAP,
            prefetch_pending_cap: PREFETCH_PENDING_CAP,
        }
    }
}

impl Limits {
    /// After an overflow, evict down to this level so the LRU sort does
    /// not run every frame while streaming through a large folder.
    fn evict_target(&self) -> usize {
        self.byte_budget * 3 / 4
    }
}

struct Slot {
    handle: egui::TextureHandle,
    path: PathBuf,
    bytes: usize,
    last_used: u64,
}

/// A dispatched-but-not-yet-uploaded decode request.
struct Pending {
    path: PathBuf,
    /// Focus epoch the request was made in; workers skip jobs whose ticket
    /// no longer matches, and `focus` reaps entries older than one bump.
    ticket: u64,
}

struct Job {
    key: TexKey,
    path: PathBuf,
    ticket: u64,
}

struct Decoded {
    key: TexKey,
    path: PathBuf,
    image: Result<egui::ColorImage, String>,
}

/// LRU texture cache with off-thread decode, prioritized queues and
/// amortized uploads.
///
/// All methods run on the UI thread; worker threads only touch the channel
/// ends and the shared epoch counter. `handle` is called per visible cell
/// per frame and stays O(1) apart from request dispatch.
pub struct Textures {
    slots: HashMap<TexKey, Slot>,
    pending: HashMap<TexKey, Pending>,
    ready: VecDeque<Decoded>,
    broken: HashMap<TexKey, PathBuf>,
    demand_tx: Sender<Job>,
    prefetch_tx: Sender<Job>,
    decoded_rx: Receiver<Decoded>,
    /// Shared focus epoch; bumped on visible-set changes so workers can
    /// skip superseded jobs. Relaxed ordering suffices — correctness never
    /// relies on the ticket alone, `sync` re-validates against `pending`.
    epoch: Arc<AtomicU64>,
    /// Visible keys of the previous `focus` call; change detection gates
    /// epoch bumps so a static sheet costs nothing.
    last_focus: Vec<TexKey>,
    limits: Limits,
    frame: u64,
    bytes: usize,
}

impl Textures {
    /// Creates the cache with production limits and starts its decode
    /// workers; they shut down when the cache is dropped.
    pub fn new() -> Self {
        Self::with_limits(Limits::default())
    }

    fn with_limits(limits: Limits) -> Self {
        let (demand_tx, demand_rx) = unbounded();
        let (prefetch_tx, prefetch_rx) = unbounded();
        let (decoded_tx, decoded_rx) = unbounded();
        let epoch = Arc::new(AtomicU64::new(0));
        for slot in 0..DECODE_WORKERS {
            let worker = Worker {
                epoch: Arc::clone(&epoch),
                demand: demand_rx.clone(),
                prefetch: prefetch_rx.clone(),
                decoded: decoded_tx.clone(),
            };
            std::thread::Builder::new()
                .name(format!("cullr-decode-{slot}"))
                .spawn(move || worker.run())
                .map_err(|error| tracing::warn!(%error, "decode worker unavailable"))
                // A missing worker degrades capacity, not correctness; the
                // pipeline tolerates fewer than DECODE_WORKERS threads.
                .ok();
        }
        Self {
            slots: HashMap::new(),
            pending: HashMap::new(),
            ready: VecDeque::new(),
            broken: HashMap::new(),
            demand_tx,
            prefetch_tx,
            decoded_rx,
            epoch,
            last_focus: Vec::new(),
            limits,
            frame: 0,
            bytes: 0,
        }
    }

    /// Texture for `key`'s image at `path`, requesting a decode on miss.
    ///
    /// Returns [`TextureState::Ready`] once paintable; otherwise the caller
    /// draws a placeholder. Passing `None` (row not ingested yet) reports
    /// plain loading without touching any queue.
    pub fn handle(&mut self, key: TexKey, path: Option<&Path>) -> TextureState {
        let Some(path) = path else {
            return TextureState::Loading;
        };
        if let Some(slot) = self.slots.get_mut(&key) {
            if slot.path == path {
                slot.last_used = self.frame;
                return TextureState::Ready(slot.handle.clone());
            }
            // Same row re-ingested from a new file version: its old pixels
            // are wrong now, so forget them before re-keying below.
            self.drop_slot(key);
        }
        if self.broken.get(&key).is_some_and(|stale| *stale == path) {
            return TextureState::Broken;
        }
        let ticket = self.epoch.load(Ordering::Relaxed);
        let needs_dispatch = match self.pending.get(&key) {
            Some(pending) => pending.path != path || pending.ticket != ticket,
            // Ticket aged out of a focus bump while the id stayed visible:
            // its worker-side job will be skipped, so re-issue it.
            None => true,
        };
        if needs_dispatch {
            self.dispatch(key, path.to_owned());
        }
        TextureState::Loading
    }

    /// Reports the texture keys currently on screen (sorted ascending),
    /// taken from the grid or loupe each frame. A change bumps the focus
    /// epoch, which cancels decodes for everything the user scrolled away
    /// from; entries for still-visible keys survive regardless of age.
    ///
    /// Cheap when the visible set is stable — the steady-state cost of a
    /// resting sheet is one slice comparison per frame.
    pub fn focus(&mut self, visible: &[TexKey]) {
        debug_assert!(
            visible.is_sorted(),
            "focus expects keys sorted by display position"
        );
        if visible != self.last_focus {
            self.last_focus.clear();
            self.last_focus.extend_from_slice(visible);
            self.epoch.fetch_add(1, Ordering::Relaxed);
        }
        let epoch = self.epoch.load(Ordering::Relaxed);
        self.pending
            .retain(|key, pending| pending.ticket == epoch || visible.binary_search(key).is_ok());
    }

    /// Queues low-priority decodes for off-screen neighbours so forward/
    /// backward scrolling finds tiles already decoded (SPEC §5.3) — grid
    /// rows around the viewport and loupe id±3 alike. Skips anything
    /// already live, broken or queued, and pauses entirely while too many
    /// decodes are outstanding — visible demand always wins the workers.
    pub fn prefetch<'p>(&mut self, requests: impl Iterator<Item = (TexKey, Option<&'p Path>)>) {
        for (key, path) in requests {
            let Some(path) = path else {
                continue;
            };
            if self.slots.contains_key(&key)
                || self.broken.contains_key(&key)
                || self
                    .pending
                    .get(&key)
                    .is_some_and(|pending| pending.path == path)
            {
                continue;
            }
            if self.pending.len() >= self.limits.prefetch_pending_cap {
                // Saturated: stop offering prefetch work this frame. The
                // band is recomputed next frame anyway.
                return;
            }
            let ticket = self.epoch.load(Ordering::Relaxed);
            self.pending.insert(
                key,
                Pending {
                    path: path.to_owned(),
                    ticket,
                },
            );
            let _sent = self.prefetch_tx.send(Job {
                key,
                path: path.to_owned(),
                ticket,
            });
        }
    }

    /// Drains decoder results and uploads at most `max_uploads` textures;
    /// call exactly once per frame before drawing.
    pub fn sync(&mut self, ctx: &egui::Context) {
        self.frame += 1;
        while let Ok(decoded) = self.decoded_rx.try_recv() {
            if self.pending.get(&decoded.key).map(|p| p.path.as_path())
                == Some(decoded.path.as_path())
            {
                self.ready.push_back(decoded);
            } else {
                // Superseded request (epoch cancelled or path changed):
                // dropped without reaching the GPU.
            }
        }
        // Bound the RAM backlog; shed the oldest (stalest) results first
        // and release their pending entry so a later visible pass can
        // re-demand them instead of waiting on a phantom job forever.
        while self.ready.len() > self.limits.ready_cap {
            let Some(shed) = self.ready.pop_front() else {
                break;
            };
            if self.pending.get(&shed.key).map(|p| p.path.as_path()) == Some(shed.path.as_path()) {
                self.pending.remove(&shed.key);
            }
        }
        for _ in 0..self.limits.max_uploads {
            let Some(decoded) = self.ready.pop_front() else {
                break;
            };
            if self.pending.get(&decoded.key).map(|p| p.path.as_path())
                != Some(decoded.path.as_path())
            {
                continue;
            }
            self.pending.remove(&decoded.key);
            match decoded.image {
                Ok(image) => self.upload(ctx, decoded.key, decoded.path, image),
                Err(error) => {
                    tracing::debug!(id = decoded.key.id.0, %error, "image decode failed");
                    self.broken.insert(decoded.key, decoded.path);
                }
            }
        }
        self.evict_over_budget();
    }

    /// `true` while decodes are queued or uploads remain; drives the UI
    /// repaint cadence until the visible sheet settles.
    pub fn busy(&self) -> bool {
        !self.pending.is_empty() || !self.ready.is_empty()
    }

    /// Forgets every texture and queue entry not in `keep`; used when a
    /// folder closes so cancelled batches stop occupying budget. Applies
    /// to both size classes of each id.
    pub fn retain(&mut self, keep: &std::collections::HashSet<PhotoId>) {
        let mut dropped_bytes = 0usize;
        self.slots.retain(|key, slot| {
            if keep.contains(&key.id) {
                true
            } else {
                dropped_bytes += slot.bytes;
                false
            }
        });
        self.bytes -= dropped_bytes;
        self.pending.retain(|key, _| keep.contains(&key.id));
        self.ready.retain(|decoded| keep.contains(&decoded.key.id));
        self.broken.retain(|key, _| keep.contains(&key.id));
    }

    fn dispatch(&mut self, key: TexKey, path: PathBuf) {
        let ticket = self.epoch.load(Ordering::Relaxed);
        self.pending.insert(
            key,
            Pending {
                path: path.clone(),
                ticket,
            },
        );
        let _sent = self.demand_tx.send(Job { key, path, ticket });
    }

    fn upload(&mut self, ctx: &egui::Context, key: TexKey, path: PathBuf, image: egui::ColorImage) {
        let [width, height] = image.size;
        let bytes = width * height * 4;
        let name = format!("{}-{}", key.class_prefix(), key.id.0);
        let handle = ctx.load_texture(&name, image, egui::TextureOptions::LINEAR);
        self.bytes += bytes;
        self.slots.insert(
            key,
            Slot {
                handle,
                path,
                bytes,
                last_used: self.frame,
            },
        );
    }

    fn drop_slot(&mut self, key: TexKey) {
        if let Some(slot) = self.slots.remove(&key) {
            self.bytes -= slot.bytes;
        }
    }

    /// LRU eviction once the byte budget overflows; slots touched most
    /// recently (including everything on screen) sort last and survive
    /// even during pathological bursts, so visible cells never flicker.
    fn evict_over_budget(&mut self) {
        if self.bytes <= self.limits.byte_budget {
            return;
        }
        let mut order: Vec<(u64, TexKey)> = self
            .slots
            .iter()
            .map(|(key, slot)| (slot.last_used, *key))
            .collect();
        order.sort_unstable();
        let mut bytes = self.bytes;
        let mut victims = 0;
        for (index, (_, key)) in order.iter().enumerate() {
            if bytes <= self.limits.evict_target() {
                break;
            }
            if let Some(slot) = self.slots.get(key) {
                bytes -= slot.bytes;
            }
            victims = index + 1;
        }
        for (_, key) in &order[..victims] {
            self.slots.remove(key);
        }
    }
}

impl Default for Textures {
    fn default() -> Self {
        Self::new()
    }
}

/// One decode worker's state: shared epoch plus its channel ends.
struct Worker {
    epoch: Arc<AtomicU64>,
    demand: Receiver<Job>,
    prefetch: Receiver<Job>,
    decoded: Sender<Decoded>,
}

impl Worker {
    /// Pull-loop: read → (skip if superseded) → decode → ship RGBA8 back
    /// for UI-thread upload. Exits when the cache (and both senders) drop.
    fn run(&self) {
        while let Some(job) = self.next_job() {
            // Epoch mismatch means the user scrolled on: skip in O(1)
            // instead of burning decode time on invisible rows.
            if self.epoch.load(Ordering::Relaxed) != job.ticket {
                continue;
            }
            let image = decode_jpeg(&job.path);
            let _sent = self.decoded.send(Decoded {
                key: job.key,
                path: job.path,
                image,
            });
        }
    }

    /// Next job preferring the demand queue; blocks only when both queues
    /// are empty. Both channels close together with the cache, so either
    /// disconnect ends the loop.
    fn next_job(&self) -> Option<Job> {
        loop {
            match self.demand.try_recv() {
                Ok(job) => return Some(job),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => return None,
            }
            match self.prefetch.try_recv() {
                Ok(job) => return Some(job),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => return None,
            }
            crossbeam_channel::select! {
                recv(self.demand) -> job => {
                    if let Ok(job) = job {
                        return Some(job);
                    }
                }
                recv(self.prefetch) -> job => {
                    if let Ok(job) = job {
                        return Some(job);
                    }
                }
            }
        }
    }
}

/// Decodes a cached thumbnail JPEG into an unmultiplied-RGBA image.
fn decode_jpeg(path: &Path) -> Result<egui::ColorImage, String> {
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    let image = image::load_from_memory(&bytes).map_err(|error| error.to_string())?;
    let rgba = image.to_rgba8();
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [rgba.width() as usize, rgba.height() as usize],
        rgba.as_raw(),
    ))
}

#[cfg(test)]
mod tests {
    #![expect(clippy::expect_used)]

    use super::*;

    struct Env {
        dir: tempfile::TempDir,
        ctx: egui::Context,
    }

    impl Env {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().expect("temp dir"),
                ctx: egui::Context::default(),
            }
        }

        fn jpeg(&self, name: &str, width: u32, height: u32) -> PathBuf {
            let path = self.dir.path().join(name);
            let image = image::RgbImage::from_pixel(width, height, image::Rgb([180, 90, 20]));
            let file = std::fs::File::create(&path).expect("create test jpeg");
            image
                .write_to(&mut std::io::BufWriter::new(file), image::ImageFormat::Jpeg)
                .expect("encode test jpeg");
            path
        }

        /// Runs `step` until it reports true or the 5 s budget expires.
        fn settle(&self, mut step: impl FnMut() -> bool) -> bool {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                if step() {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            false
        }
    }

    fn textures_with(limits: Limits) -> Textures {
        Textures::with_limits(limits)
    }

    fn prod_limits() -> Limits {
        Limits::default()
    }

    #[test]
    fn uploads_should_respect_the_per_frame_cap() {
        let env = Env::new();
        let files: Vec<PathBuf> = (0..20)
            .map(|index| env.jpeg(&format!("up-{index}.jpg"), 8, 6))
            .collect();
        let mut textures = textures_with(prod_limits());

        for (index, file) in files.iter().enumerate() {
            textures.handle(TexKey::thumb(PhotoId(index as u64)), Some(file));
        }
        let mut synced_frames = 0;
        let all_ready = env.settle(|| {
            let before = textures.slots.len();
            textures.sync(&env.ctx);
            let uploaded = textures.slots.len() - before;
            assert!(
                uploaded <= MAX_UPLOADS_PER_FRAME,
                "uploaded {uploaded} in one frame"
            );
            synced_frames += 1;
            textures.slots.len() == files.len()
        });

        assert!(
            all_ready,
            "never reached {}/{} ready",
            textures.slots.len(),
            files.len()
        );
        assert!(synced_frames >= files.len() / MAX_UPLOADS_PER_FRAME);
    }

    #[test]
    fn eviction_should_drop_the_least_recently_used_first() {
        let env = Env::new();
        // 16×12 RGBA8 = 768 B per texture; the budget fits two plus slack
        // and its eviction target keeps one survivor, so a third upload
        // forces exactly one LRU eviction.
        let limits = Limits {
            byte_budget: 2200,
            ..prod_limits()
        };
        let a = env.jpeg("a.jpg", 16, 12);
        let b = env.jpeg("b.jpg", 16, 12);
        let c = env.jpeg("c.jpg", 16, 12);
        let mut textures = textures_with(limits);

        // Load 1 and 2 sequentially so their recency stamps differ by
        // whole frames regardless of worker scheduling.
        textures.handle(TexKey::thumb(PhotoId(1)), Some(&a));
        let _ = env.settle(|| {
            textures.sync(&env.ctx);
            textures.slots.contains_key(&TexKey::thumb(PhotoId(1)))
        });
        textures.handle(TexKey::thumb(PhotoId(2)), Some(&b));
        let _ = env.settle(|| {
            textures.sync(&env.ctx);
            textures.slots.contains_key(&TexKey::thumb(PhotoId(2)))
        });
        // Advance one frame so the touch below stamps strictly newer
        // recency than 2's upload frame (ties fall back to id order).
        textures.sync(&env.ctx);
        // Touch 1 well after 2's upload so its recency clearly wins.
        textures.handle(TexKey::thumb(PhotoId(1)), Some(&a));
        textures.handle(TexKey::thumb(PhotoId(3)), Some(&c));
        let settled = env.settle(|| {
            textures.sync(&env.ctx);
            !textures.busy()
        });

        assert!(settled, "third texture never arrived");
        assert!(
            textures.slots.contains_key(&TexKey::thumb(PhotoId(1))),
            "recently used evicted"
        );
        assert!(
            textures.slots.contains_key(&TexKey::thumb(PhotoId(3))),
            "fresh upload evicted"
        );
        assert!(
            !textures.slots.contains_key(&TexKey::thumb(PhotoId(2))),
            "LRU victim survived"
        );
    }

    #[test]
    fn scrolled_past_requests_should_be_cancelled_by_focus() {
        let env = Env::new();
        let file = env.jpeg("cancel.jpg", 8, 6);
        let mut textures = textures_with(prod_limits());

        textures.handle(TexKey::thumb(PhotoId(7)), Some(&file));
        // The user scrolls: id 7 leaves the viewport, another id shows up.
        textures.focus(&[TexKey::thumb(PhotoId(8))]);
        let drained = env.settle(|| {
            textures.sync(&env.ctx);
            !textures.busy()
        });

        assert!(drained, "stale request kept the pipeline busy");
        assert!(textures.slots.is_empty(), "cancelled work reached the GPU");

        // Scrolled back into view: a fresh demand re-loads it.
        textures.handle(TexKey::thumb(PhotoId(7)), Some(&file));
        let loaded = env.settle(|| {
            textures.sync(&env.ctx);
            matches!(
                textures.handle(TexKey::thumb(PhotoId(7)), Some(&file)),
                TextureState::Ready(_)
            )
        });
        assert!(loaded, "re-demanded texture never became ready");
    }

    #[test]
    fn backlog_overflow_should_shed_old_results_but_recover_on_redemand() {
        let env = Env::new();
        let files: Vec<PathBuf> = (0..8)
            .map(|index| env.jpeg(&format!("burst-{index}.jpg"), 8, 6))
            .collect();
        let limits = Limits {
            ready_cap: 1,
            max_uploads: 1,
            ..prod_limits()
        };
        let mut textures = textures_with(limits);

        for (index, file) in files.iter().enumerate() {
            textures.handle(TexKey::thumb(PhotoId(index as u64)), Some(file));
        }
        // Let both workers finish all eight sub-millisecond decodes before
        // the first sync, so the backlog overflows deterministically.
        std::thread::sleep(std::time::Duration::from_millis(100));
        // Upload trickles at 1/frame while the full batch sits in RAM, so
        // the cap must shed results; whatever was shed lost its pending
        // entry and would spin forever unless re-demanded.
        env.settle(|| {
            textures.sync(&env.ctx);
            !textures.busy()
        });
        let missing: Vec<usize> = (0..files.len())
            .filter(|index| {
                !textures
                    .slots
                    .contains_key(&TexKey::thumb(PhotoId(*index as u64)))
            })
            .collect();

        // Re-demanding the shed ids must recover every one of them.
        let all_recovered = env.settle(|| {
            for index in &missing {
                textures.handle(TexKey::thumb(PhotoId(*index as u64)), Some(&files[*index]));
            }
            textures.sync(&env.ctx);
            missing.iter().all(|index| {
                matches!(
                    textures.handle(TexKey::thumb(PhotoId(*index as u64)), Some(&files[*index])),
                    TextureState::Ready(_)
                )
            })
        });

        assert!(!missing.is_empty(), "expected the cap to shed something");
        assert!(all_recovered, "shed ids did not recover after re-demand");
    }

    #[test]
    fn prefetch_should_pause_once_the_pending_queue_is_saturated() {
        let env = Env::new();
        let one = env.jpeg("one.jpg", 8, 6);
        let two = env.jpeg("two.jpg", 8, 6);
        let three = env.jpeg("three.jpg", 8, 6);
        let limits = Limits {
            prefetch_pending_cap: 2,
            ..prod_limits()
        };
        let mut textures = textures_with(limits);

        textures.handle(TexKey::thumb(PhotoId(1)), Some(&one));
        textures.prefetch(
            [
                (TexKey::thumb(PhotoId(2)), Some(two.as_path())),
                (TexKey::thumb(PhotoId(3)), Some(three.as_path())),
            ]
            .into_iter(),
        );

        assert_eq!(
            textures.pending.len(),
            2,
            "prefetch ignored the saturation cap"
        );
        assert!(textures.pending.contains_key(&TexKey::thumb(PhotoId(1))));
        assert!(textures.pending.contains_key(&TexKey::thumb(PhotoId(2))));
        assert!(!textures.pending.contains_key(&TexKey::thumb(PhotoId(3))));
    }

    #[test]
    fn reingest_with_a_new_thumb_path_should_replace_stale_pixels() {
        let env = Env::new();
        let old = env.jpeg("old.jpg", 8, 6);
        let new = env.jpeg("new.jpg", 10, 6);
        let mut textures = textures_with(prod_limits());
        let id = PhotoId(5);
        let key = TexKey::thumb(id);

        textures.handle(key, Some(&old));
        let loaded = env.settle(|| {
            textures.sync(&env.ctx);
            matches!(textures.handle(key, Some(&old)), TextureState::Ready(_))
        });
        assert!(loaded, "initial load failed");

        textures.handle(key, Some(&new));
        assert!(matches!(
            textures.handle(key, Some(&new)),
            TextureState::Loading
        ));
        let swapped = env.settle(|| {
            textures.sync(&env.ctx);
            matches!(textures.handle(key, Some(&new)), TextureState::Ready(_))
        });

        assert!(swapped, "re-ingested texture never became ready");
        assert_eq!(textures.slots[&key].path, new, "slot kept the stale asset");
    }

    #[test]
    fn undecodable_jpeg_should_report_broken_without_retry_churn() {
        let env = Env::new();
        let corrupt = env.dir.path().join("corrupt.jpg");
        std::fs::write(&corrupt, b"not a jpeg").expect("write corrupt file");
        let mut textures = textures_with(prod_limits());
        let id = PhotoId(9);
        let key = TexKey::thumb(id);

        textures.handle(key, Some(&corrupt));
        let marked = env.settle(|| {
            textures.sync(&env.ctx);
            matches!(textures.handle(key, Some(&corrupt)), TextureState::Broken)
        });

        assert!(marked, "decode failure never surfaced as Broken");
        assert!(!textures.busy(), "broken asset keeps retrying");
    }

    #[test]
    fn thumb_and_screen_classes_should_cache_independently_for_one_id() {
        let env = Env::new();
        let thumb = env.jpeg("t.jpg", 8, 6);
        let screen = env.jpeg("s.jpg", 32, 24);
        let mut textures = textures_with(prod_limits());
        let id = PhotoId(3);
        let thumb_key = TexKey::thumb(id);
        let screen_key = TexKey::screen(id);

        textures.handle(thumb_key, Some(&thumb));
        textures.handle(screen_key, Some(&screen));
        let both_ready = env.settle(|| {
            textures.sync(&env.ctx);
            matches!(
                textures.handle(thumb_key, Some(&thumb)),
                TextureState::Ready(_)
            ) && matches!(
                textures.handle(screen_key, Some(&screen)),
                TextureState::Ready(_)
            )
        });

        assert!(both_ready, "the two classes never became ready together");
        assert_eq!(textures.slots.len(), 2, "one class evicted the other");
        assert_ne!(
            textures.slots[&thumb_key].handle.id(),
            textures.slots[&screen_key].handle.id(),
            "classes shared one texture"
        );
    }

    #[test]
    fn focus_should_cancel_only_the_requested_class() {
        let env = Env::new();
        let file = env.jpeg("cls.jpg", 8, 6);
        let mut textures = textures_with(prod_limits());
        let id = PhotoId(4);
        let thumb_key = TexKey::thumb(id);
        let screen_key = TexKey::screen(id);

        // A thumb decode is in flight when the loupe takes over the focus;
        // the epoch bump must cancel the off-screen thumb class.
        textures.handle(thumb_key, Some(&file));
        textures.focus(&[screen_key]);
        let settled = env.settle(|| {
            textures.sync(&env.ctx);
            !textures.busy()
        });

        assert!(settled);
        assert!(
            textures.slots.is_empty(),
            "cancelled thumb class reached the GPU"
        );

        // The focused screen class still loads normally afterwards.
        textures.handle(screen_key, Some(&file));
        let loaded = env.settle(|| {
            textures.sync(&env.ctx);
            matches!(
                textures.handle(screen_key, Some(&file)),
                TextureState::Ready(_)
            )
        });
        assert!(loaded, "focused screen key never loaded");
    }

    #[test]
    /// SPEC §8 stall gate: bursting 10k thumbnail decodes through the
    /// manager must keep every UI-thread `sync` below the 50 ms stall
    /// ceiling (release mode, like the other perf gates):
    ///
    /// ```text
    /// CULLR_PERF=<dir> cargo test --release -p cullr-ui -- tex --ignored
    /// ```
    #[ignore = "perf gate: run with CULLR_PERF set, release mode"]
    fn ten_k_burst_scroll_should_keep_sync_under_the_stall_ceiling() {
        let Some(scratch_base) = std::env::var_os("CULLR_PERF").map(std::path::PathBuf::from)
        else {
            return;
        };
        let env_dir = tempfile::tempdir_in(scratch_base).expect("scratch dir");
        let files: Vec<PathBuf> = (0..10_000u32)
            .map(|index| {
                let path = env_dir.path().join(format!("t{index:05}.jpg"));
                let image = image::RgbImage::from_pixel(24, 16, image::Rgb([index as u8, 90, 20]));
                let file = std::fs::File::create(&path).expect("create perf jpeg");
                image
                    .write_to(&mut std::io::BufWriter::new(file), image::ImageFormat::Jpeg)
                    .expect("encode perf jpeg");
                path
            })
            .collect();

        let ctx = egui::Context::default();
        let mut textures = Textures::new();
        for (index, file) in files.iter().enumerate() {
            textures.handle(TexKey::thumb(PhotoId(index as u64)), Some(file));
        }

        let started = std::time::Instant::now();
        let mut worst = std::time::Duration::ZERO;
        let mut frames = 0usize;

        // One UI frame of the pipeline: upload freshly decoded textures
        // (`sync`), then draw, which (re-)demands whatever is visible.
        fn frame(textures: &mut Textures, ctx: &egui::Context, worst: &mut std::time::Duration) {
            let started = std::time::Instant::now();
            textures.sync(ctx);
            *worst = (*worst).max(started.elapsed());
            assert!(
                started.elapsed() < std::time::Duration::from_millis(50),
                "sync stalled the frame: {:?}",
                started.elapsed()
            );
        }

        // Burst phase: every cell demands at once, the worst case for both
        // backlog pressure and eviction churn.
        while textures.busy() {
            frame(&mut textures, &ctx, &mut worst);
            frames += 1;
        }
        let burst_frames = frames;

        // Shed results were parked by the backlog cap, exactly as for a
        // user who outran the pipeline; re-demanding until resident models
        // scrolling back over them and proves nothing was lost.
        while textures.slots.len() < files.len() {
            assert!(frames < 200_000, "re-demand did not converge");
            for (index, file) in files.iter().enumerate() {
                textures.handle(TexKey::thumb(PhotoId(index as u64)), Some(file));
            }
            while textures.busy() {
                frame(&mut textures, &ctx, &mut worst);
                frames += 1;
            }
        }

        println!(
            "10k burst drained in {:?} ({burst_frames} burst + {} recovery frames); \
             worst sync {worst:?} vs 50 ms stall ceiling",
            started.elapsed(),
            frames - burst_frames,
        );
        assert_eq!(textures.slots.len(), files.len(), "not every thumb landed");
    }

    #[test]
    /// SPEC §8 loupe gate: opening the loupe on a 24 MP preview must stay
    /// under 400 ms cold (disk read + JPEG decode) and 30 ms warm (texture
    /// resident). Release mode, like the other perf gates:
    ///
    /// ```text
    /// CULLR_PERF=<dir> cargo test --release -p cullr-ui -- loupe --ignored
    /// ```
    #[ignore = "perf gate: run with CULLR_PERF set, release mode"]
    fn loupe_open_should_meet_the_cold_and_warm_budgets() {
        let Some(scratch_base) = std::env::var_os("CULLR_PERF").map(std::path::PathBuf::from)
        else {
            return;
        };
        let env_dir = tempfile::tempdir_in(scratch_base).expect("scratch dir");
        // 6000×4000 matches a full-size modern camera embedded preview.
        let mut pixels = image::RgbImage::new(6000, 4000);
        for (x, y, pixel) in pixels.enumerate_pixels_mut() {
            *pixel = image::Rgb([(x % 255) as u8, (y % 255) as u8, ((x + y) % 255) as u8]);
        }
        let path = env_dir.path().join("loupe-perf.jpg");
        let file = std::fs::File::create(&path).expect("create perf jpeg");
        let encoder =
            image::codecs::jpeg::JpegEncoder::new_with_quality(std::io::BufWriter::new(file), 88);
        pixels
            .write_with_encoder(encoder)
            .expect("encode perf jpeg");

        let ctx = egui::Context::default();
        let key = TexKey::screen(PhotoId(1));

        // Cold: demand → worker decode → upload must fit in 400 ms.
        let mut textures = Textures::new();
        let cold_started = std::time::Instant::now();
        textures.handle(key, Some(&path));
        let ready = loop {
            textures.sync(&ctx);
            if matches!(textures.handle(key, Some(&path)), TextureState::Ready(_)) {
                break true;
            }
            if cold_started.elapsed() > std::time::Duration::from_secs(10) {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        let cold = cold_started.elapsed();
        assert!(ready, "screen texture never became ready");
        assert!(
            cold < std::time::Duration::from_millis(400),
            "cold loupe open took {cold:?} vs 400 ms budget"
        );

        // Warm: the slot is resident; a full open pass is just bookkeeping.
        let warm_started = std::time::Instant::now();
        for _ in 0..100 {
            textures.focus(&[key]);
            assert!(matches!(
                textures.handle(key, Some(&path)),
                TextureState::Ready(_)
            ));
            textures.sync(&ctx);
        }
        let warm = warm_started.elapsed() / 100;
        assert!(
            warm < std::time::Duration::from_millis(30),
            "warm loupe open took {warm:?} vs 30 ms budget"
        );
        println!("loupe open: cold {cold:?} / warm {warm:?}");
    }
}
