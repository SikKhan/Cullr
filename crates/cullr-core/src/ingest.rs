//! Ingest pipeline (SPEC §5.2): a shared job queue drained by a dedicated
//! rayon pool, with generation-based cancellation and panic isolation.
//!
//! One [`IngestPipeline`] lives for the whole app session. Opening a folder
//! enqueues its pending photos as a new *generation*; any job of an older
//! generation is dropped without touching the database, which is what makes
//! a mid-run folder switch cancel cleanly. Every finished or dropped job
//! decrements its batch counter, so consumers always get exactly one
//! [`IngestEvent::Finished`] per batch — even when cancellation purged most
//! of it before a worker ever started.
//!
//! Failure policy: extraction errors and panics both degrade to an error row
//! plus [`IngestEvent::Failed`], never aborting the pool (SPEC §7).
//!
//! ```
//! use std::sync::Arc;
//!
//! use cullr_core::{Cache, Db, IngestPipeline};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let dir = tempfile::tempdir()?;
//! let db = Arc::new(Db::open(&dir.path().join("index.db"))?);
//! let (pipeline, _events) = IngestPipeline::new(Cache::new(dir.path().join("cache")), db);
//!
//! // Enqueuing nothing is a no-op that still reports the live generation.
//! assert_eq!(pipeline.enqueue(Vec::new()), 0);
//! # Ok(())
//! # }
//! ```

use std::collections::{HashSet, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crossbeam_channel::{Receiver, Sender, unbounded};
use rayon::{ThreadPool, ThreadPoolBuilder};

use crate::cache::Cache;
use crate::db::Db;
use crate::extract::{extract_file, panic_message};
use crate::model::{PendingPhoto, PhotoId};

/// Upper bound on ingest workers (SPEC §3: `num_cpus`, capped at 12).
const MAX_INGEST_WORKERS: usize = 12;

/// Progress events streamed to the UI while a batch runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngestEvent {
    /// A photo extracted successfully; its row is now [`crate::PhotoStatus::Ok`].
    Ingested(PhotoId),
    /// A photo failed permanently for this file version; its row carries an
    /// error message and the pipeline moved on (SPEC §5.2 step 6).
    Failed(PhotoId),
    /// The last job of this generation was processed or dropped.
    Finished {
        /// Generation the completed batch was enqueued under.
        generation: u64,
    },
}

/// Driver over pending-photo extraction: queue, worker pool, cancellation
/// and priority control. Cheap to clone internally via `Arc`; all methods
/// take `&self`.
pub struct IngestPipeline {
    inner: Arc<PipelineInner>,
}

struct PipelineInner {
    cache: Cache,
    db: Arc<Db>,
    sender: Sender<IngestEvent>,
    queue: Mutex<VecDeque<Job>>,
    generation: AtomicU64,
}

/// Completion accounting for one enqueued batch.
struct Batch {
    generation: u64,
    remaining: AtomicUsize,
    sender: Sender<IngestEvent>,
}

struct Job {
    batch: Arc<Batch>,
    photo: PendingPhoto,
}

impl IngestPipeline {
    /// Creates a pipeline writing assets into `cache` and index rows into
    /// `db`; progress events arrive on the returned receiver until it is
    /// dropped.
    pub fn new(cache: Cache, db: Arc<Db>) -> (Self, Receiver<IngestEvent>) {
        let (sender, receiver) = unbounded();
        let inner = PipelineInner {
            cache,
            db,
            sender,
            queue: Mutex::new(VecDeque::new()),
            generation: AtomicU64::new(0),
        };
        (
            Self {
                inner: Arc::new(inner),
            },
            receiver,
        )
    }

    /// Enqueues photos as the next generation and starts workers; returns
    /// the new generation id. An empty list changes nothing.
    pub fn enqueue(&self, photos: Vec<PendingPhoto>) -> u64 {
        if photos.is_empty() {
            return self.active_generation();
        }
        let generation = self.inner.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let batch = Arc::new(Batch {
            generation,
            remaining: AtomicUsize::new(photos.len()),
            sender: self.inner.sender.clone(),
        });
        let queued = {
            let mut queue = locked_queue(&self.inner);
            queue.extend(photos.into_iter().map(|photo| Job {
                batch: Arc::clone(&batch),
                photo,
            }));
            queue.len()
        };
        for _ in 0..max_workers().min(queued) {
            let inner = Arc::clone(&self.inner);
            spawn_worker(inner);
        }
        tracing::debug!(generation, jobs = queued, "ingest batch enqueued");
        generation
    }

    /// Cancels every outstanding batch: queued jobs are dropped at once and
    /// in-flight ones drop their results between steps, so a mid-run folder
    /// switch leaves only rows the old batch had already committed.
    pub fn cancel(&self) {
        self.inner.generation.fetch_add(1, Ordering::Relaxed);
        for job in locked_queue(&self.inner).drain(..) {
            job.finish();
        }
    }

    /// Moves the given photos to the front of the queue so near-viewport
    /// items jump ahead (SPEC §5.2 visible-window priority ping); relative
    /// order follows queue position, unknown ids are ignored. Already
    /// in-flight jobs cannot be reordered; at most `MAX_INGEST_WORKERS` can
    /// be running at once.
    pub fn prioritize(&self, ids: &[PhotoId]) {
        if ids.is_empty() {
            return;
        }
        let wanted: HashSet<u64> = ids.iter().map(|id| id.0).collect();
        let mut queue = locked_queue(&self.inner);
        let mut head = VecDeque::with_capacity(queue.len());
        let mut tail = VecDeque::with_capacity(queue.len());
        for job in queue.drain(..) {
            if wanted.contains(&job.photo.id.0) {
                head.push_back(job);
            } else {
                tail.push_back(job);
            }
        }
        head.extend(tail);
        *queue = head;
    }

    /// Generation the next [`Self::enqueue`] call would produce.
    pub fn active_generation(&self) -> u64 {
        self.inner.generation.load(Ordering::Relaxed)
    }
}

impl std::fmt::Debug for IngestPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IngestPipeline")
            .field("generation", &self.active_generation())
            .finish()
    }
}

fn locked_queue(inner: &PipelineInner) -> std::sync::MutexGuard<'_, VecDeque<Job>> {
    // Adopted on poison: batches keep their accounting consistent even if a
    // worker panicked while holding the lock, and bricking the pipeline over
    // one panic would violate SPEC §7.
    inner.queue.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Pull-loop run on the rayon pool: pops jobs until the queue is empty, then
/// exits. Extra loops spawned by a later batch exit immediately when they
/// find nothing to do.
fn worker_loop(inner: Arc<PipelineInner>) {
    loop {
        let job = {
            let mut queue = locked_queue(&inner);
            queue.pop_front()
        };
        match job {
            Some(job) => run_job(&inner, job),
            None => return,
        }
    }
}

/// Processes one job with full panic isolation: anything unwinding through
/// rawler or our own glue becomes a typed error row instead of killing the
/// worker thread (SPEC §5.2 step 6).
fn run_job(inner: &PipelineInner, job: Job) {
    let outcome = catch_unwind(AssertUnwindSafe(|| process(inner, &job)));
    if let Err(message) = outcome.unwrap_or_else(|payload| Err(panic_message(&payload))) {
        if !is_stale(inner, job.batch.generation) {
            if let Err(error) = inner.db.record_ingest_error(job.photo.id, &message) {
                tracing::warn!(%error, "cannot record ingest error");
            }
            let _ = job.batch.sender.send(IngestEvent::Failed(job.photo.id));
        }
    }
    job.finish();
}

/// Runs one extraction with cancellation checks around the slow section:
/// results computed for a superseded generation are dropped instead of being
/// written to the index.
fn process(inner: &PipelineInner, job: &Job) -> Result<(), String> {
    if is_stale(inner, job.batch.generation) {
        return Ok(());
    }
    // extract_file covers open → metadata → fallback chain → cache writes,
    // each under catch_unwind; checking either side of it is the practical
    // granularity because rawler calls are not interruptible mid-flight.
    let info = extract_file(&job.photo.meta, &inner.cache).map_err(|error| error.to_string())?;
    if is_stale(inner, job.batch.generation) {
        return Ok(());
    }
    inner
        .db
        .record_ingest_ok(job.photo.id, &info)
        .map_err(|error| error.to_string())?;
    let _ = job.batch.sender.send(IngestEvent::Ingested(job.photo.id));
    Ok(())
}

impl Job {
    /// Reports one processed-or-dropped job; the final decrement emits the
    /// batch's single `Finished` event. Every job path must call this
    /// exactly once: `run_job` at its end, and `cancel` for each purged job.
    fn finish(self) {
        if self.batch.remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
            let _ = self.batch.sender.send(IngestEvent::Finished {
                generation: self.batch.generation,
            });
        }
    }
}

fn is_stale(inner: &PipelineInner, generation: u64) -> bool {
    inner.generation.load(Ordering::Relaxed) != generation
}

/// The dedicated ingest pool (SPEC §3), when it could be built.
fn dedicated_pool() -> Option<&'static ThreadPool> {
    static POOL: std::sync::OnceLock<Option<ThreadPool>> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        match ThreadPoolBuilder::new()
            .num_threads(max_workers())
            .thread_name(|slot| format!("cullr-ingest-{slot}"))
            .build()
        {
            Ok(pool) => Some(pool),
            Err(error) => {
                tracing::warn!(%error, "dedicated ingest pool unavailable; using global pool");
                None
            }
        }
    })
    .as_ref()
}

/// Runs a worker pull-loop on the ingest pool, falling back to rayon's
/// global pool when the dedicated one could not be created.
fn spawn_worker(inner: Arc<PipelineInner>) {
    match dedicated_pool() {
        Some(pool) => pool.spawn(move || worker_loop(inner)),
        None => rayon::spawn(move || worker_loop(inner)),
    }
}

fn max_workers() -> usize {
    std::thread::available_parallelism()
        .map_or(4, |cpus| cpus.get())
        .clamp(1, MAX_INGEST_WORKERS)
}

#[cfg(test)]
mod tests {
    // Test setup asserts hard failures; a broken fixture aborts the test.
    #![expect(clippy::expect_used)]

    use std::fs;
    use std::time::Duration;

    use crossbeam_channel::RecvTimeoutError;
    use tempfile::TempDir;

    use super::*;
    use crate::model::PhotoStatus;
    use crate::scanner::{ScanOptions, scan_folder};

    /// Wait-for-completion budget; generous so slow CI machines never flake.
    const DRAIN_TIMEOUT: Duration = Duration::from_secs(120);

    struct Fixture {
        _dir: TempDir,
        db: Arc<Db>,
        root: std::path::PathBuf,
    }

    /// Builds a scanned+synced folder of `count` files whose content rawler
    /// cannot decode: the pipeline exercises its full error path without
    /// needing vendor fixtures.
    fn fixture_with(count: usize) -> Fixture {
        let dir = TempDir::new().expect("temp dir");
        for index in 0..count {
            fs::write(dir.path().join(format!("IMG_{index:04}.nef")), b"not a raw")
                .expect("test setup write");
        }
        let db = Arc::new(Db::open(&dir.path().join("index.db")).expect("open db"));
        let scanned = scan_folder(dir.path(), ScanOptions::default()).expect("scan");
        db.sync_scan(dir.path(), &scanned, ScanOptions::default())
            .expect("sync");
        let root = dir.path().to_owned();
        Fixture {
            _dir: dir,
            db,
            root,
        }
    }

    fn pending_of(fixture: &Fixture) -> Vec<PendingPhoto> {
        fixture.db.pending_photos(&fixture.root).expect("pending")
    }

    fn pipeline_for(fixture: &Fixture) -> (IngestPipeline, Receiver<IngestEvent>) {
        IngestPipeline::new(
            Cache::new(fixture._dir.path().join("cache")),
            Arc::clone(&fixture.db),
        )
    }

    /// Seeds the queue directly with one shared batch and no workers, so
    /// queue-manipulation tests run fully deterministically.
    fn seed_queue(pipeline: &IngestPipeline, photos: &[PendingPhoto]) {
        let batch = Arc::new(Batch {
            generation: pipeline.active_generation(),
            remaining: AtomicUsize::new(photos.len()),
            sender: pipeline.inner.sender.clone(),
        });
        locked_queue(&pipeline.inner).extend(photos.iter().map(|photo| Job {
            batch: Arc::clone(&batch),
            photo: photo.clone(),
        }));
    }

    /// Collects events until `Finished` reports `generation`, returning them
    /// in arrival order (with the terminator last).
    fn drain_until(receiver: &Receiver<IngestEvent>, generation: u64) -> Vec<IngestEvent> {
        let mut events = Vec::new();
        loop {
            match receiver.recv_timeout(DRAIN_TIMEOUT) {
                Ok(event) => {
                    let done = matches!(
                        &event,
                        IngestEvent::Finished { generation: done } if *done == generation
                    );
                    events.push(event);
                    if done {
                        return events;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    panic!("timed out waiting for Finished {{ generation: {generation} }}");
                }
                Err(RecvTimeoutError::Disconnected) => panic!("event channel disconnected"),
            }
        }
    }

    fn status_count(fixture: &Fixture, status: PhotoStatus) -> i64 {
        fixture
            .db
            .with_conn(|conn| {
                Ok(conn.query_row(
                    "SELECT count(*) FROM photos WHERE status = ?1",
                    [status.to_u8()],
                    |row| row.get(0),
                )?)
            })
            .expect("status query")
    }

    #[test]
    fn empty_enqueue_should_be_a_no_op_returning_the_active_generation() {
        let fx = fixture_with(0);
        let (pipeline, receiver) = pipeline_for(&fx);

        assert_eq!(pipeline.enqueue(Vec::new()), 0);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn enqueue_should_process_every_photo_and_finish_exactly_once() {
        let fx = fixture_with(8);
        let (pipeline, receiver) = pipeline_for(&fx);
        let pending = pending_of(&fx);
        let ids: HashSet<u64> = pending.iter().map(|p| p.id.0).collect();

        let generation = pipeline.enqueue(pending);
        let events = drain_until(&receiver, generation);

        assert_eq!(generation, 1);
        assert_eq!(status_count(&fx, PhotoStatus::Error), 8);
        let finished = events
            .iter()
            .filter(|e| matches!(e, IngestEvent::Finished { .. }))
            .count();
        assert_eq!(finished, 1);
        let failed_ids: HashSet<u64> = events
            .into_iter()
            .filter_map(|e| match e {
                IngestEvent::Failed(id) => Some(id.0),
                _ => None,
            })
            .collect();
        assert_eq!(failed_ids, ids);
    }

    #[test]
    fn cancel_should_stop_a_running_batch_and_leave_the_pipeline_usable() {
        // The T6 done-criterion proxy: a 500-file folder, cancelled mid-run,
        // must quiesce cleanly and still ingest to completion afterwards.
        let fx = fixture_with(500);
        let (pipeline, receiver) = pipeline_for(&fx);

        let first = pipeline.enqueue(pending_of(&fx));
        std::thread::sleep(Duration::from_millis(20));
        pipeline.cancel();
        drain_until(&receiver, first);

        let second = pipeline.enqueue(pending_of(&fx));
        drain_until(&receiver, second);

        assert!(second > first);
        assert_eq!(status_count(&fx, PhotoStatus::Error), 500);
        assert_eq!(status_count(&fx, PhotoStatus::Pending), 0);
        assert_eq!(status_count(&fx, PhotoStatus::Ok), 0);
    }

    #[test]
    fn enqueue_should_supersede_the_previous_batch_without_explicit_cancel() {
        let fx = fixture_with(300);
        let (pipeline, receiver) = pipeline_for(&fx);

        let stale = pipeline.enqueue(pending_of(&fx));
        let fresh = pipeline.enqueue(pending_of(&fx));
        drain_until(&receiver, stale);
        drain_until(&receiver, fresh);

        assert_eq!(fresh, stale + 1);
        assert_eq!(status_count(&fx, PhotoStatus::Pending), 0);
    }

    #[test]
    fn cancel_should_emit_finished_even_when_no_job_ever_started() {
        let fx = fixture_with(3);
        let (pipeline, receiver) = pipeline_for(&fx);
        let generation = pipeline.active_generation();
        seed_queue(&pipeline, &pending_of(&fx));

        pipeline.cancel();

        assert_eq!(
            drain_until(&receiver, generation),
            vec![IngestEvent::Finished { generation }]
        );
    }

    #[test]
    fn prioritize_should_move_requested_jobs_to_the_front_preserving_queue_order() {
        let fx = fixture_with(5);
        let (pipeline, _receiver) = pipeline_for(&fx);
        let photos = pending_of(&fx);
        seed_queue(&pipeline, &photos);

        pipeline.prioritize(&[photos[3].id, photos[0].id]);

        let order: Vec<u64> = locked_queue(&pipeline.inner)
            .iter()
            .map(|job| job.photo.id.0)
            .collect();
        assert_eq!(
            order,
            vec![
                photos[0].id.0,
                photos[3].id.0,
                photos[1].id.0,
                photos[2].id.0,
                photos[4].id.0
            ]
        );
    }

    #[test]
    fn prioritize_should_tolerate_unknown_and_empty_requests() {
        let fx = fixture_with(2);
        let (pipeline, _receiver) = pipeline_for(&fx);
        let photos = pending_of(&fx);
        seed_queue(&pipeline, &photos);

        pipeline.prioritize(&[]);
        pipeline.prioritize(&[PhotoId(9_999)]);

        let order: Vec<u64> = locked_queue(&pipeline.inner)
            .iter()
            .map(|job| job.photo.id.0)
            .collect();
        assert_eq!(order, vec![photos[0].id.0, photos[1].id.0]);
    }

    #[test]
    fn max_workers_should_stay_within_the_spec_cap() {
        assert!(max_workers() >= 1);
        assert!(max_workers() <= MAX_INGEST_WORKERS);
    }
}
