//! GPU texture pipeline for grid/loupe imagery (SPEC §5.3).
//!
//! Cached thumbnail JPEGs are decoded on small worker threads — turned
//! upright per their EXIF flag and any user rotation — and uploaded to
//! the GPU here, on the UI thread, at a capped rate per frame. Live
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
use image::DynamicImage;

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

/// Display rotation applied when a cached JPEG is decoded: the photo's
/// EXIF orientation flag plus any user quarter-turns clockwise on top.
///
/// Cached assets keep the sensor orientation they were embedded with;
/// turning them upright is a presentation concern, so it happens here —
/// once per decode, off the UI thread. Changing either field invalidates
/// the resident slot exactly like a re-ingested asset path does, which is
/// what makes manual rotation refresh tiles without cache surgery.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Rotation {
    /// EXIF orientation flag (`1..8`, TIFF spec); values outside the range
    /// decode as upright.
    pub exif: u16,
    /// Extra quarter-turns clockwise (`0..4`).
    pub cw_turns: u8,
}

impl Rotation {
    /// No rotation at all.
    pub const UPRIGHT: Self = Self {
        exif: 1,
        cw_turns: 0,
    };

    /// Builds the transform for one photo row.
    pub fn new(exif: u16, cw_turns: u8) -> Self {
        Self {
            exif,
            cw_turns: cw_turns % 4,
        }
    }
}

impl Default for Rotation {
    fn default() -> Self {
        Self::UPRIGHT
    }
}

/// Turns a stored-orientation image into display pixels: first the EXIF
/// correction (mirrors and quarter-turns per the TIFF spec), then the
/// user's clockwise turns. Pure so the mapping is unit-testable against
/// known pixel matrices.
fn orient_upright(mut image: DynamicImage, rotation: Rotation) -> DynamicImage {
    match rotation.exif {
        2 => image = image.fliph(),
        3 => image = image.rotate180(),
        4 => image = image.flipv(),
        // 5 (transpose) and 7 (transverse): the two diagonal mirrors are a
        // quarter turn composed with an edge flip.
        5 => {
            image = image.rotate90();
            image = image.fliph();
        }
        6 => image = image.rotate90(),
        7 => {
            image = image.rotate90();
            image = image.flipv();
        }
        8 => image = image.rotate270(),
        _ => {}
    }
    for _ in 0..rotation.cw_turns.min(3) {
        image = image.rotate90();
    }
    image
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
    /// not run every frame while streaming through a large folder. The
    /// band between target and budget (25 % ≈ 128 MB in production) is
    /// sized to absorb a long burst of uploads between eviction passes:
    /// over a hundred grid thumbs at ~1 MB RGBA8 each, or more than one
    /// worst-case loupe screen — enough hysteresis that steady scrolling
    /// costs one sort per ~128 MB uploaded instead of per frame.
    fn evict_target(&self) -> usize {
        self.byte_budget * 3 / 4
    }
}

struct Slot {
    handle: egui::TextureHandle,
    path: PathBuf,
    rotation: Rotation,
    bytes: usize,
    last_used: u64,
}

/// A dispatched-but-not-yet-uploaded decode request.
struct Pending {
    path: PathBuf,
    rotation: Rotation,
    /// Focus epoch the request was made in; workers skip jobs whose ticket
    /// no longer matches, and `focus` reaps entries older than one bump.
    ticket: u64,
}

struct Job {
    key: TexKey,
    path: PathBuf,
    rotation: Rotation,
    ticket: u64,
}

struct Decoded {
    key: TexKey,
    path: PathBuf,
    rotation: Rotation,
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
    broken: HashMap<TexKey, (PathBuf, Rotation)>,
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

    /// Texture for `key`'s image at `path`, displayed with `rotation`,
    /// requesting a decode on miss.
    ///
    /// Returns [`TextureState::Ready`] once paintable; otherwise the caller
    /// draws a placeholder. Passing `None` (row not ingested yet) reports
    /// plain loading without touching any queue.
    pub fn handle(&mut self, key: TexKey, path: Option<&Path>, rotation: Rotation) -> TextureState {
        let Some(path) = path else {
            return TextureState::Loading;
        };
        if let Some(slot) = self.slots.get_mut(&key) {
            if slot.path == path && slot.rotation == rotation {
                slot.last_used = self.frame;
                return TextureState::Ready(slot.handle.clone());
            }
            // Same row re-ingested from a new file version — or manually
            // rotated: its old pixels are wrong now, so forget them before
            // re-keying below.
            self.drop_slot(key);
        }
        if self
            .broken
            .get(&key)
            .is_some_and(|stale| *stale == (path.to_owned(), rotation))
        {
            return TextureState::Broken;
        }
        let ticket = self.epoch.load(Ordering::Relaxed);
        let needs_dispatch = match self.pending.get(&key) {
            Some(pending) => {
                pending.path != path || pending.rotation != rotation || pending.ticket != ticket
            }
            // Ticket aged out of a focus bump while the id stayed visible:
            // its worker-side job will be skipped, so re-issue it.
            None => true,
        };
        if needs_dispatch {
            self.dispatch(key, path.to_owned(), rotation);
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
    pub fn prefetch<'p>(
        &mut self,
        requests: impl Iterator<Item = (TexKey, Option<(&'p Path, Rotation)>)>,
    ) {
        for (key, request) in requests {
            let Some((path, rotation)) = request else {
                continue;
            };
            if self.slots.contains_key(&key)
                || self.broken.contains_key(&key)
                || self
                    .pending
                    .get(&key)
                    .is_some_and(|pending| pending.path == path && pending.rotation == rotation)
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
                    rotation,
                    ticket,
                },
            );
            let _sent = self.prefetch_tx.send(Job {
                key,
                path: path.to_owned(),
                rotation,
                ticket,
            });
        }
    }

    /// Drains decoder results and uploads at most `max_uploads` textures;
    /// call exactly once per frame before drawing.
    pub fn sync(&mut self, ctx: &egui::Context) {
        self.frame += 1;
        while let Ok(decoded) = self.decoded_rx.try_recv() {
            if self.pending.get(&decoded.key).is_some_and(|pending| {
                pending.path == decoded.path && pending.rotation == decoded.rotation
            }) {
                self.ready.push_back(decoded);
            } else {
                // Superseded request (epoch cancelled or path/rotation
                // changed): dropped without reaching the GPU.
            }
        }
        // Bound the RAM backlog; shed the oldest (stalest) results first
        // and release their pending entry so a later visible pass can
        // re-demand them instead of waiting on a phantom job forever.
        while self.ready.len() > self.limits.ready_cap {
            let Some(shed) = self.ready.pop_front() else {
                break;
            };
            if self.pending.get(&shed.key).is_some_and(|pending| {
                pending.path == shed.path && pending.rotation == shed.rotation
            }) {
                self.pending.remove(&shed.key);
            }
        }
        for _ in 0..self.limits.max_uploads {
            let Some(decoded) = self.ready.pop_front() else {
                break;
            };
            let matches_pending = self.pending.get(&decoded.key).is_some_and(|pending| {
                pending.path == decoded.path && pending.rotation == decoded.rotation
            });
            if !matches_pending {
                // Superseded between backlog and upload: drop silently.
                continue;
            }
            self.pending.remove(&decoded.key);
            let Decoded {
                key,
                path,
                rotation,
                image,
            } = decoded;
            match image {
                Ok(image) => self.upload(ctx, key, path, rotation, image),
                Err(error) => {
                    tracing::debug!(id = key.id.0, %error, "image decode failed");
                    self.broken.insert(key, (path, rotation));
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

    fn dispatch(&mut self, key: TexKey, path: PathBuf, rotation: Rotation) {
        let ticket = self.epoch.load(Ordering::Relaxed);
        self.pending.insert(
            key,
            Pending {
                path: path.clone(),
                rotation,
                ticket,
            },
        );
        let _sent = self.demand_tx.send(Job {
            key,
            path,
            rotation,
            ticket,
        });
    }

    fn upload(
        &mut self,
        ctx: &egui::Context,
        key: TexKey,
        path: PathBuf,
        rotation: Rotation,
        image: egui::ColorImage,
    ) {
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
                rotation,
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
        // Evicted bytes must leave `self.bytes` too; counting them only in
        // this local sum would inflate the counter permanently, silently
        // shrinking the effective budget towards the evict target and
        // turning every later sync into an unnecessary eviction pass.
        let mut evicted = 0usize;
        let mut victims = 0;
        for (index, (_, key)) in order.iter().enumerate() {
            if bytes <= self.limits.evict_target() {
                break;
            }
            if let Some(slot) = self.slots.get(key) {
                bytes -= slot.bytes;
                evicted += slot.bytes;
            }
            victims = index + 1;
        }
        for (_, key) in &order[..victims] {
            self.slots.remove(key);
        }
        self.bytes -= evicted;
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
    /// Pull-loop: read → (skip if superseded) → decode + rotate → ship
    /// RGBA8 back for UI-thread upload. Exits when the cache (and both
    /// senders) drop.
    fn run(&self) {
        while let Some(job) = self.next_job() {
            // Epoch mismatch means the user scrolled on: skip in O(1)
            // instead of burning decode time on invisible rows.
            if self.epoch.load(Ordering::Relaxed) != job.ticket {
                continue;
            }
            // Rotation happens here, off the UI thread, while the decoded
            // buffer is already in memory; a portrait screen preview pays
            // one extra buffer copy per open instead of every frame.
            let image =
                decode_jpeg(&job.path).map(|image| colorize(orient_upright(image, job.rotation)));
            let _sent = self.decoded.send(Decoded {
                key: job.key,
                path: job.path,
                rotation: job.rotation,
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

/// Decodes a cached thumbnail JPEG into its stored pixels.
fn decode_jpeg(path: &Path) -> Result<DynamicImage, String> {
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    image::load_from_memory(&bytes).map_err(|error| error.to_string())
}

/// Converts display-ready pixels into an unmultiplied-RGBA GPU image.
fn colorize(image: DynamicImage) -> egui::ColorImage {
    let rgba = image.to_rgba8();
    egui::ColorImage::from_rgba_unmultiplied(
        [rgba.width() as usize, rgba.height() as usize],
        rgba.as_raw(),
    )
}

#[cfg(test)]
mod tests {
    #![expect(clippy::expect_used)]

    use std::collections::HashSet;

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
            textures.handle(
                TexKey::thumb(PhotoId(index as u64)),
                Some(file),
                Rotation::UPRIGHT,
            );
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
        textures.handle(TexKey::thumb(PhotoId(1)), Some(&a), Rotation::UPRIGHT);
        let _ = env.settle(|| {
            textures.sync(&env.ctx);
            textures.slots.contains_key(&TexKey::thumb(PhotoId(1)))
        });
        textures.handle(TexKey::thumb(PhotoId(2)), Some(&b), Rotation::UPRIGHT);
        let _ = env.settle(|| {
            textures.sync(&env.ctx);
            textures.slots.contains_key(&TexKey::thumb(PhotoId(2)))
        });
        // Advance one frame so the touch below stamps strictly newer
        // recency than 2's upload frame (ties fall back to id order).
        textures.sync(&env.ctx);
        // Touch 1 well after 2's upload so its recency clearly wins.
        textures.handle(TexKey::thumb(PhotoId(1)), Some(&a), Rotation::UPRIGHT);
        textures.handle(TexKey::thumb(PhotoId(3)), Some(&c), Rotation::UPRIGHT);
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
    fn eviction_should_keep_recently_visible_textures() {
        let env = Env::new();
        // 16×12 RGBA8 = 768 B per texture; seven competing uploads overflow
        // the 2400 B budget repeatedly, forcing evictions down to the 1800 B
        // target while the burst streams through.
        let limits = Limits {
            byte_budget: 2400,
            ..prod_limits()
        };
        let files: Vec<PathBuf> = (0..7)
            .map(|index| env.jpeg(&format!("vis-{index}.jpg"), 16, 12))
            .collect();
        let mut textures = textures_with(limits);
        for (index, file) in files.iter().enumerate() {
            textures.handle(
                TexKey::thumb(PhotoId(index as u64)),
                Some(file),
                Rotation::UPRIGHT,
            );
        }
        // Highest id also wins recency ties, mirroring a cell that stays on
        // screen while everything around it scrolls past.
        let hot = TexKey::thumb(PhotoId(999));

        let settled = env.settle(|| {
            // Re-demanding the hot key refreshes its recency every frame,
            // exactly like a visible cell repainted by the grid.
            textures.handle(
                hot,
                Some(files.last().expect("non-empty").as_path()),
                Rotation::UPRIGHT,
            );
            textures.sync(&env.ctx);
            !textures.busy()
        });

        assert!(settled, "burst never settled");
        assert!(
            textures.slots.contains_key(&hot),
            "recently visible texture was evicted during the burst"
        );
    }

    #[test]
    fn eviction_should_settle_at_the_hysteresis_level_and_keep_survivors() {
        let env = Env::new();
        // Six 16×12 textures = 4608 B overflow the 2200 B budget; eviction
        // must stop once bytes reach its 1650 B target instead of running
        // to zero.
        let limits = Limits {
            byte_budget: 2200,
            ..prod_limits()
        };
        let evict_target = limits.evict_target();
        let files: Vec<PathBuf> = (0..6)
            .map(|index| env.jpeg(&format!("hyst-{index}.jpg"), 16, 12))
            .collect();
        let mut textures = textures_with(limits);
        for (index, file) in files.iter().enumerate() {
            textures.handle(
                TexKey::thumb(PhotoId(index as u64)),
                Some(file),
                Rotation::UPRIGHT,
            );
        }
        let _ = env.settle(|| {
            textures.sync(&env.ctx);
            !textures.busy()
        });
        // One extra pass so the assertions observe post-eviction state even
        // if settle returned on the same frame as the final upload.
        textures.sync(&env.ctx);

        assert!(
            textures.bytes <= evict_target,
            "eviction overshot the hysteresis level: {} B resident vs {evict_target} B target",
            textures.bytes
        );
        assert!(
            !textures.slots.is_empty(),
            "hysteresis eviction emptied the cache entirely"
        );
    }

    #[test]
    fn retain_should_drop_non_kept_ids_from_every_cache_layer() {
        let env = Env::new();
        let kept_file = env.jpeg("kept.jpg", 8, 6);
        let gone_file = env.jpeg("gone.jpg", 8, 6);
        let mut textures = textures_with(prod_limits());
        let kept = TexKey::thumb(PhotoId(1));
        let gone = TexKey::thumb(PhotoId(2));

        textures.handle(kept, Some(&kept_file), Rotation::UPRIGHT);
        textures.handle(gone, Some(&gone_file), Rotation::UPRIGHT);
        let _ = env.settle(|| {
            textures.sync(&env.ctx);
            textures.slots.len() == 2
        });

        let keep: HashSet<PhotoId> = [PhotoId(1)].into_iter().collect();
        textures.retain(&keep);

        assert!(
            !textures.slots.contains_key(&gone),
            "closed-folder texture survived retain"
        );
        assert!(
            !textures.pending.contains_key(&gone),
            "closed-folder decode job survived retain"
        );
        assert_eq!(
            textures.bytes, textures.slots[&kept].bytes,
            "byte accounting drifted after retain"
        );
        assert!(
            !textures.busy(),
            "closed-folder work kept the pipeline busy"
        );
    }

    #[test]
    fn prefetch_should_skip_keys_already_resident_or_in_flight() {
        let env = Env::new();
        let file = env.jpeg("dedup.jpg", 8, 6);
        let mut textures = textures_with(prod_limits());
        let key = TexKey::thumb(PhotoId(6));

        textures.handle(key, Some(&file), Rotation::UPRIGHT);
        textures.prefetch([(key, Some((file.as_path(), Rotation::UPRIGHT)))].into_iter());

        assert_eq!(
            textures.pending.len(),
            1,
            "prefetch duplicated an in-flight demand"
        );

        let _ = env.settle(|| {
            textures.sync(&env.ctx);
            matches!(
                textures.handle(key, Some(&file), Rotation::UPRIGHT),
                TextureState::Ready(_)
            )
        });
        textures.prefetch([(key, Some((file.as_path(), Rotation::UPRIGHT)))].into_iter());

        assert!(!textures.busy(), "prefetch requeued a resident texture");
    }

    #[test]
    fn scrolled_past_requests_should_be_cancelled_by_focus() {
        let env = Env::new();
        let file = env.jpeg("cancel.jpg", 8, 6);
        let mut textures = textures_with(prod_limits());

        textures.handle(TexKey::thumb(PhotoId(7)), Some(&file), Rotation::UPRIGHT);
        // The user scrolls: id 7 leaves the viewport, another id shows up.
        textures.focus(&[TexKey::thumb(PhotoId(8))]);
        let drained = env.settle(|| {
            textures.sync(&env.ctx);
            !textures.busy()
        });

        assert!(drained, "stale request kept the pipeline busy");
        assert!(textures.slots.is_empty(), "cancelled work reached the GPU");

        // Scrolled back into view: a fresh demand re-loads it.
        textures.handle(TexKey::thumb(PhotoId(7)), Some(&file), Rotation::UPRIGHT);
        let loaded = env.settle(|| {
            textures.sync(&env.ctx);
            matches!(
                textures.handle(TexKey::thumb(PhotoId(7)), Some(&file), Rotation::UPRIGHT),
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
            textures.handle(
                TexKey::thumb(PhotoId(index as u64)),
                Some(file),
                Rotation::UPRIGHT,
            );
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
                textures.handle(
                    TexKey::thumb(PhotoId(*index as u64)),
                    Some(&files[*index]),
                    Rotation::UPRIGHT,
                );
            }
            textures.sync(&env.ctx);
            missing.iter().all(|index| {
                matches!(
                    textures.handle(
                        TexKey::thumb(PhotoId(*index as u64)),
                        Some(&files[*index]),
                        Rotation::UPRIGHT
                    ),
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

        textures.handle(TexKey::thumb(PhotoId(1)), Some(&one), Rotation::UPRIGHT);
        textures.prefetch(
            [
                (
                    TexKey::thumb(PhotoId(2)),
                    Some((two.as_path(), Rotation::UPRIGHT)),
                ),
                (
                    TexKey::thumb(PhotoId(3)),
                    Some((three.as_path(), Rotation::UPRIGHT)),
                ),
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

        textures.handle(key, Some(&old), Rotation::UPRIGHT);
        let loaded = env.settle(|| {
            textures.sync(&env.ctx);
            matches!(
                textures.handle(key, Some(&old), Rotation::UPRIGHT),
                TextureState::Ready(_)
            )
        });
        assert!(loaded, "initial load failed");

        textures.handle(key, Some(&new), Rotation::UPRIGHT);
        assert!(matches!(
            textures.handle(key, Some(&new), Rotation::UPRIGHT),
            TextureState::Loading
        ));
        let swapped = env.settle(|| {
            textures.sync(&env.ctx);
            matches!(
                textures.handle(key, Some(&new), Rotation::UPRIGHT),
                TextureState::Ready(_)
            )
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

        textures.handle(key, Some(&corrupt), Rotation::UPRIGHT);
        let marked = env.settle(|| {
            textures.sync(&env.ctx);
            matches!(
                textures.handle(key, Some(&corrupt), Rotation::UPRIGHT),
                TextureState::Broken
            )
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

        textures.handle(thumb_key, Some(&thumb), Rotation::UPRIGHT);
        textures.handle(screen_key, Some(&screen), Rotation::UPRIGHT);
        let both_ready = env.settle(|| {
            textures.sync(&env.ctx);
            matches!(
                textures.handle(thumb_key, Some(&thumb), Rotation::UPRIGHT),
                TextureState::Ready(_)
            ) && matches!(
                textures.handle(screen_key, Some(&screen), Rotation::UPRIGHT),
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
        textures.handle(thumb_key, Some(&file), Rotation::UPRIGHT);
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
        textures.handle(screen_key, Some(&file), Rotation::UPRIGHT);
        let loaded = env.settle(|| {
            textures.sync(&env.ctx);
            matches!(
                textures.handle(screen_key, Some(&file), Rotation::UPRIGHT),
                TextureState::Ready(_)
            )
        });
        assert!(loaded, "focused screen key never loaded");
    }

    // --- EXIF + manual rotation mapping ---

    /// Builds a 2×2 image whose pixels read `[A B; C D]`, so every
    /// transform below can be asserted against a hand-derived matrix.
    fn marker_image() -> DynamicImage {
        let colors = [
            [255, 0, 0],   // A
            [0, 255, 0],   // B
            [0, 0, 255],   // C
            [255, 255, 0], // D
        ];
        let mut image = image::RgbImage::new(2, 2);
        for (index, color) in colors.iter().enumerate() {
            image.put_pixel((index % 2) as u32, (index / 2) as u32, image::Rgb(*color));
        }
        DynamicImage::ImageRgb8(image)
    }

    fn pixel(image: &DynamicImage, x: u32, y: u32) -> [u8; 3] {
        let rgb = image.to_rgb8();
        rgb.get_pixel(x, y).0
    }

    #[test]
    fn orient_upright_should_match_the_exif_transform_table() {
        let cases: [(u16, [[u8; 3]; 4]); 8] = [
            // 1: stored pixels are already upright.
            (1, [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 0]]),
            // 2: mirrored horizontally.
            (2, [[0, 255, 0], [255, 0, 0], [255, 255, 0], [0, 0, 255]]),
            // 3: rotated 180°.
            (3, [[255, 255, 0], [0, 0, 255], [0, 255, 0], [255, 0, 0]]),
            // 4: mirrored vertically.
            (4, [[0, 0, 255], [255, 255, 0], [255, 0, 0], [0, 255, 0]]),
            // 5: transpose over the main diagonal.
            (5, [[255, 0, 0], [0, 0, 255], [0, 255, 0], [255, 255, 0]]),
            // 6: quarter turn clockwise.
            (6, [[0, 0, 255], [255, 0, 0], [255, 255, 0], [0, 255, 0]]),
            // 7: transverse (anti-diagonal mirror).
            (7, [[255, 255, 0], [0, 255, 0], [0, 0, 255], [255, 0, 0]]),
            // 8: quarter turn counter-clockwise.
            (8, [[0, 255, 0], [255, 255, 0], [255, 0, 0], [0, 0, 255]]),
        ];
        for (orientation, expected) in cases {
            let out = orient_upright(marker_image(), Rotation::new(orientation, 0));
            for y in 0..2 {
                for x in 0..2 {
                    assert_eq!(
                        pixel(&out, x, y),
                        expected[(y * 2 + x) as usize],
                        "orientation {orientation} pixel ({x},{y})"
                    );
                }
            }
        }
    }

    #[test]
    fn user_turns_should_compose_with_the_exif_correction_sequentially() {
        // A portrait EXIF flag plus one further CW turn is a half turn.
        let combined = orient_upright(marker_image(), Rotation::new(6, 1));
        let half = orient_upright(marker_image(), Rotation::new(3, 0));
        assert_eq!(combined.to_rgba8().into_raw(), half.to_rgba8().into_raw());

        // Four turns would be a no-op; three land back on one CCW turn.
        let ccw = orient_upright(marker_image(), Rotation::new(1, 3));
        let once_cw = orient_upright(marker_image(), Rotation::new(1, 1));
        assert_ne!(ccw.to_rgba8().into_raw(), once_cw.to_rgba8().into_raw());
    }

    #[test]
    fn rotating_a_visible_photo_should_replace_its_texture() {
        let env = Env::new();
        let file = env.jpeg("rot.jpg", 16, 12);
        let mut textures = textures_with(prod_limits());
        let key = TexKey::thumb(PhotoId(11));

        textures.handle(key, Some(&file), Rotation::UPRIGHT);
        let loaded = env.settle(|| {
            textures.sync(&env.ctx);
            matches!(
                textures.handle(key, Some(&file), Rotation::UPRIGHT),
                TextureState::Ready(_)
            )
        });
        assert!(loaded, "initial load failed");

        // The user presses ]: same asset path, new display rotation. The
        // stale slot must be dropped and a rotated decode queued.
        assert!(matches!(
            textures.handle(key, Some(&file), Rotation::new(1, 1)),
            TextureState::Loading
        ));
        let rotated = env.settle(|| {
            textures.sync(&env.ctx);
            matches!(
                textures.handle(key, Some(&file), Rotation::new(1, 1)),
                TextureState::Ready(_)
            )
        });
        assert!(rotated, "rotated texture never became ready");
        assert_eq!(textures.slots[&key].rotation, Rotation::new(1, 1));
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
            textures.handle(
                TexKey::thumb(PhotoId(index as u64)),
                Some(file),
                Rotation::UPRIGHT,
            );
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
                textures.handle(
                    TexKey::thumb(PhotoId(index as u64)),
                    Some(file),
                    Rotation::UPRIGHT,
                );
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
        textures.handle(key, Some(&path), Rotation::UPRIGHT);
        let ready = loop {
            textures.sync(&ctx);
            if matches!(
                textures.handle(key, Some(&path), Rotation::UPRIGHT),
                TextureState::Ready(_)
            ) {
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
                textures.handle(key, Some(&path), Rotation::UPRIGHT),
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
