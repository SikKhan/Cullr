//! GPU texture pipeline for grid/loupe imagery (SPEC §5.3 groundwork).
//!
//! Cached thumbnail JPEGs are decoded on small worker threads and uploaded
//! to the GPU here, on the UI thread, at a capped rate per frame. Live
//! textures form an LRU keyed by photo id with a byte budget, so only
//! recently seen cells own GPU memory (SPEC key invariant) and scrolling a
//! huge folder cannot exhaust VRAM.
//!
//! Correctness across re-ingest: slots remember which asset path they were
//! built from. When an id's thumb path changes (file edited → new cache
//! hash), the stale slot is dropped and the asset re-decodes; results for
//! superseded requests are discarded without ever reaching the GPU.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use crossbeam_channel::{Receiver, Sender, unbounded};
use eframe::egui;

use cullr_core::PhotoId;

/// Byte ceiling for live textures (SPEC §8: 512 MB GPU texture budget).
const BYTE_BUDGET: usize = 512 * 1024 * 1024;
/// After an overflow, evict down to this level so the LRU sort does not run
/// every frame while streaming through a large folder.
const EVICT_TARGET: usize = BYTE_BUDGET * 3 / 4;
/// New GPU uploads per frame (SPEC §5.3: amortized, ≤ 6).
const MAX_UPLOADS_PER_FRAME: usize = 6;
/// Background JPEG decoders feeding [`Textures`]; decode of a ≤512 px JPEG
/// is sub-millisecond, so two workers outpace both ingest and upload caps.
const DECODE_WORKERS: usize = 2;

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

struct Slot {
    handle: egui::TextureHandle,
    path: PathBuf,
    bytes: usize,
    last_used: u64,
}

struct Decoded {
    id: PhotoId,
    path: PathBuf,
    image: Result<egui::ColorImage, String>,
}

/// LRU texture cache with off-thread decode and amortized uploads.
///
/// All methods run on the UI thread; the two worker threads only touch the
/// channel ends. `handle` is called per visible cell per frame and must
/// stay O(1) apart from request dispatch.
pub struct Textures {
    slots: HashMap<PhotoId, Slot>,
    /// Latest requested path per id: dedups in-flight work and validates
    /// results still match what the current index wants.
    pending: HashMap<PhotoId, PathBuf>,
    ready: VecDeque<Decoded>,
    broken: HashMap<PhotoId, PathBuf>,
    requests_tx: Sender<(PhotoId, PathBuf)>,
    decoded_rx: Receiver<Decoded>,
    frame: u64,
    bytes: usize,
}

impl Textures {
    /// Creates the cache and starts its decode workers; they shut down
    /// when the cache is dropped.
    pub fn new() -> Self {
        let (requests_tx, requests_rx) = unbounded();
        let (decoded_tx, decoded_rx) = unbounded();
        for slot in 0..DECODE_WORKERS {
            let requests_rx = requests_rx.clone();
            let decoded_tx = decoded_tx.clone();
            std::thread::Builder::new()
                .name(format!("cullr-decode-{slot}"))
                .spawn(move || decode_loop(requests_rx, decoded_tx))
                .map_err(|error| tracing::warn!(%error, "decode worker unavailable"))
                // A missing worker degrades capacity, not correctness; the
                // loop below tolerates fewer than DECODE_WORKERS threads.
                .ok();
        }
        Self {
            slots: HashMap::new(),
            pending: HashMap::new(),
            ready: VecDeque::new(),
            broken: HashMap::new(),
            requests_tx,
            decoded_rx,
            frame: 0,
            bytes: 0,
        }
    }

    /// Texture for `id`'s thumbnail at `path`, requesting a decode on miss.
    ///
    /// Returns [`TextureState::Ready`] once painted-able; otherwise the
    /// caller draws a placeholder. Passing `None` (row not ingested yet)
    /// reports plain loading without touching any queue.
    pub fn handle(&mut self, id: PhotoId, path: Option<&Path>) -> TextureState {
        let Some(path) = path else {
            return TextureState::Loading;
        };
        if let Some(slot) = self.slots.get_mut(&id) {
            if slot.path == path {
                slot.last_used = self.frame;
                return TextureState::Ready(slot.handle.clone());
            }
            // Same row re-ingested from a new file version: its old pixels
            // are wrong now, so forget them before re-keying below.
            self.drop_slot(id);
        }
        if self.broken.get(&id).is_some_and(|stale| *stale == path) {
            return TextureState::Broken;
        }
        if !self.pending.get(&id).is_some_and(|queued| *queued == path) {
            self.pending.insert(id, path.to_owned());
            let _sent = self.requests_tx.send((id, path.to_owned()));
        }
        TextureState::Loading
    }

    /// Drains decoder results and uploads at most
    /// [`MAX_UPLOADS_PER_FRAME`] textures; call exactly once per frame
    /// before drawing.
    pub fn sync(&mut self, ctx: &egui::Context) {
        self.frame += 1;
        while let Ok(decoded) = self.decoded_rx.try_recv() {
            if self.pending.get(&decoded.id) == Some(&decoded.path) {
                self.ready.push_back(decoded);
            }
            // Mismatched results are for superseded requests: dropped.
        }
        for _ in 0..MAX_UPLOADS_PER_FRAME {
            let Some(decoded) = self.ready.pop_front() else {
                break;
            };
            if self.pending.get(&decoded.id).map(|p| p.as_path()) != Some(decoded.path.as_path()) {
                continue;
            }
            self.pending.remove(&decoded.id);
            match decoded.image {
                Ok(image) => self.upload(ctx, decoded.id, decoded.path, image),
                Err(error) => {
                    tracing::debug!(id = decoded.id.0, %error, "thumb decode failed");
                    self.broken.insert(decoded.id, decoded.path);
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
    /// folder closes so cancelled batches stop occupying budget.
    pub fn retain(&mut self, keep: &std::collections::HashSet<PhotoId>) {
        let mut dropped_bytes = 0usize;
        self.slots.retain(|id, slot| {
            if keep.contains(id) {
                true
            } else {
                dropped_bytes += slot.bytes;
                false
            }
        });
        self.bytes -= dropped_bytes;
        self.pending.retain(|id, _| keep.contains(id));
        self.ready.retain(|decoded| keep.contains(&decoded.id));
        self.broken.retain(|id, _| keep.contains(id));
    }

    fn upload(&mut self, ctx: &egui::Context, id: PhotoId, path: PathBuf, image: egui::ColorImage) {
        let [width, height] = image.size;
        let bytes = width * height * 4;
        let name = format!("thumb-{}", id.0);
        let handle = ctx.load_texture(&name, image, egui::TextureOptions::LINEAR);
        self.bytes += bytes;
        self.slots.insert(
            id,
            Slot {
                handle,
                path,
                bytes,
                last_used: self.frame,
            },
        );
    }

    fn drop_slot(&mut self, id: PhotoId) {
        if let Some(slot) = self.slots.remove(&id) {
            self.bytes -= slot.bytes;
        }
    }

    /// LRU eviction once the byte budget overflows; slots touched this
    /// frame survive even during pathological bursts so visible cells
    /// never flicker.
    fn evict_over_budget(&mut self) {
        if self.bytes <= BYTE_BUDGET {
            return;
        }
        let mut order: Vec<(u64, PhotoId)> = self
            .slots
            .iter()
            .filter(|(_, slot)| slot.last_used < self.frame)
            .map(|(id, slot)| (slot.last_used, *id))
            .collect();
        order.sort_unstable();
        let mut bytes = self.bytes;
        let mut victims = 0;
        for (index, (_, id)) in order.iter().enumerate() {
            if bytes <= EVICT_TARGET {
                break;
            }
            if let Some(slot) = self.slots.get(id) {
                bytes -= slot.bytes;
            }
            victims = index + 1;
        }
        for (_, id) in &order[..victims] {
            self.slots.remove(id);
        }
    }
}

impl Default for Textures {
    fn default() -> Self {
        Self::new()
    }
}

/// Worker pull-loop: read → decode → ship RGBA8 back for UI-thread upload.
/// Exits when the cache (and its request channel) is dropped.
fn decode_loop(requests: Receiver<(PhotoId, PathBuf)>, decoded: Sender<Decoded>) {
    while let Ok((id, path)) = requests.recv() {
        let image = decode_jpeg(&path);
        let _sent = decoded.send(Decoded { id, path, image });
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
