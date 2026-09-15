//! Off-thread decode: the queue that orders it, and the cache that stops two tabs
//! decoding one file.
//!
//! # Why a queue, and why not tokio
//!
//! Milestone 1 had one file and one `std::thread::spawn` per load, with a note that
//! a real queue was the demosaic suite's problem. Tabs bring it forward: dropping eight
//! files on the window used to mean eight concurrent decodes, and a decode holds a
//! full `SensorImage` **and** a full `SceneImage` — on a 100 MP frame that is ~600
//! MB of transient allocation each. Eight at once is not a slow app, it is an
//! out-of-memory one.
//!
//! So the queue is owned here rather than delegated to an async runtime:
//!
//! - **Bounded concurrency**, because the bound is the point.
//! - **Priority**, so the tab the user is looking at decodes first.
//! - **Cancellation**, so closing a tab or superseding a request stops the work.
//!
//! That is the whole of what Lightbox's "bounded concurrency, priority for visible
//! tiles, cancel-on-scroll" asks for, and none of it needs `async`. `scene::decode`
//! is CPU-bound and internally rayon-parallel; wrapping a blocking rayon call in a
//! future buys nothing but a runtime. tokio stays a live option for Lightbox if
//! thousands of queued thumbnails prove a thread-per-worker model wrong, but it
//! would be adopted on that evidence rather than on anticipation.
//!
//! The queue is generic over its key and payload because Lightbox is the next
//! consumer and its key is a thumbnail, not a tab.

use std::collections::HashMap;
use std::hash::Hash;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, Weak};

use raw_core::{DecodeOptions, SceneImage, SensorImage, scene};

/// How many decodes may run at once.
///
/// **Two, and the reason is not throughput.** `scene::decode` is rayon-parallel
/// internally and already saturates every core, so a second concurrent decode does
/// not finish the pair any sooner — it overlaps one decode's file read with
/// another's arithmetic, which is the only real win available. Each extra worker
/// costs a full sensor plus a full scene of transient memory, so the number is
/// small deliberately.
pub const WORKERS: usize = 2;

/// Priority classes. Lower runs first.
///
/// The foreground tab is what the user is looking at; everything else can wait,
/// however it was ordered. Same idea Lightbox will need for visible tiles.
pub const FOREGROUND: u32 = 0;
pub const BACKGROUND: u32 = 10;

// ---------------------------------------------------------------- content keying

/// Identity of a raw file, independent of where it sits on disk.
///
/// **Path is deliberately not part of it.** Saving a duplicate copies the raw
/// (reflink where the filesystem allows), and the copy is byte-identical and
/// immutable — so it must share its original's decode rather than repeat it. A
/// path-keyed cache cannot see that; neither can an mtime-keyed one, because a copy
/// need not preserve mtime.
///
/// The handoff offered "hash, or size+mtime+camera serial". This is the first
/// option, sampled rather than complete: exact length plus three 64 KiB windows.
/// Hashing 200 MB in full costs ~100 ms on every open in order to notice a file we
/// may already have, which is the wrong trade; the head window alone already
/// differs between two frames from one camera, since it carries the timestamp and
/// the embedded preview, and the tail is image data.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ContentKey {
    len: u64,
    sample: u64,
}

/// Bytes read at each of the three probe points.
const WINDOW: usize = 64 * 1024;

impl ContentKey {
    pub fn of(path: &Path) -> std::io::Result<Self> {
        let mut f = std::fs::File::open(path)?;
        let len = f.metadata()?.len();

        // FNV-1a over the three windows in order. Not cryptographic and does not
        // need to be — this distinguishes files, it does not authenticate them.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut buf = vec![0u8; WINDOW];
        let mid = len.saturating_sub(WINDOW as u64) / 2;
        let tail = len.saturating_sub(WINDOW as u64);
        for start in [0, mid, tail] {
            f.seek(SeekFrom::Start(start))?;
            // A short read is expected on a file smaller than a window and on the
            // last window of any file; hash what is there.
            let n = read_up_to(&mut f, &mut buf)?;
            for b in &buf[..n] {
                h ^= u64::from(*b);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        Ok(Self { len, sample: h })
    }
}

/// `Read::read` may return fewer bytes than asked for without being at EOF, so a
/// single call is not a window.
fn read_up_to(f: &mut std::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

// ----------------------------------------------------------------------- results

/// One decode's output, shared between every tab that wants it.
pub struct Decoded {
    pub opts: DecodeOptions,
    pub scene: SceneImage,
    pub clipped_fraction: f32,
    pub decode_ms: u128,
}

/// What a worker hands back.
pub enum Done {
    /// A file was read: the sensor is kept so decode-option changes do not re-read
    /// it, and the first decode comes with it.
    Loaded {
        sensor: Arc<SensorImage>,
        decoded: Arc<Decoded>,
        key: ContentKey,
        path: PathBuf,
    },
    /// Decode options changed and the cached sensor was re-decoded. No file read.
    Redecoded {
        decoded: Arc<Decoded>,
        key: ContentKey,
    },
    Failed(String),
}

pub fn run_decode(sensor: &SensorImage, opts: DecodeOptions) -> Decoded {
    let started = std::time::Instant::now();
    let (scene, mask) = scene::decode(sensor, opts);
    let clipped = mask.0.iter().filter(|c| **c).count() as f32 / mask.0.len() as f32;
    Decoded {
        opts,
        scene,
        clipped_fraction: clipped,
        decode_ms: started.elapsed().as_millis(),
    }
}

// ------------------------------------------------------------------------- cache

/// Shares decoded work between tabs **without keeping it alive**.
///
/// Every entry is a `Weak`, which is the whole design. The memory story the handoff
/// committed to is "N decoded scene images in RAM" where N counts *distinct* images
/// open — so a closed tab's scene has to go away, and a strong cache would quietly
/// break that by holding every file ever opened. Weak entries mean two tabs on one
/// file share one decode, nothing outlives the last tab that wants it, and there is
/// no eviction policy to tune or get wrong.
#[derive(Default)]
pub struct Cache {
    sensors: HashMap<ContentKey, Weak<SensorImage>>,
    scenes: HashMap<(ContentKey, DecodeOptions), Weak<Decoded>>,
}

impl Cache {
    pub fn sensor(&self, key: ContentKey) -> Option<Arc<SensorImage>> {
        self.sensors.get(&key)?.upgrade()
    }

    pub fn decoded(&self, key: ContentKey, opts: DecodeOptions) -> Option<Arc<Decoded>> {
        self.scenes.get(&(key, opts))?.upgrade()
    }

    pub fn put_sensor(&mut self, key: ContentKey, sensor: &Arc<SensorImage>) {
        self.sensors.insert(key, Arc::downgrade(sensor));
    }

    pub fn put_decoded(&mut self, key: ContentKey, d: &Arc<Decoded>) {
        self.scenes.insert((key, d.opts), Arc::downgrade(d));
    }

    /// Drop entries whose value is gone. Cheap, and worth doing on tab close so the
    /// maps do not grow a dead entry per file for the life of the session.
    pub fn sweep(&mut self) {
        self.sensors.retain(|_, w| w.strong_count() > 0);
        self.scenes.retain(|_, w| w.strong_count() > 0);
    }

    /// Live entries. Only a test asks — from outside, a weak cache has no
    /// observable size.
    #[cfg(test)]
    pub fn len(&self) -> (usize, usize) {
        (
            self.sensors
                .values()
                .filter(|w| w.strong_count() > 0)
                .count(),
            self.scenes
                .values()
                .filter(|w| w.strong_count() > 0)
                .count(),
        )
    }
}

// ------------------------------------------------------------------------- queue

struct Job<K, T> {
    key: K,
    priority: u32,
    /// Submission order, so equal priorities stay FIFO.
    seq: u64,
    cancel: Arc<AtomicBool>,
    work: Box<dyn FnOnce() -> T + Send>,
}

struct Shared<K, T> {
    pending: Mutex<Vec<Job<K, T>>>,
    signal: Condvar,
    stop: AtomicBool,
    /// Jobs running right now, and the high-water mark. Observable so a test can
    /// assert the bound is a bound.
    live: AtomicUsize,
    peak: AtomicUsize,
}

struct Completed<K, T> {
    key: K,
    cancel: Arc<AtomicBool>,
    out: Result<T, String>,
}

/// A bounded, prioritised, cancellable work queue over a fixed thread pool.
pub struct Queue<K: Copy + Eq + Hash + Send + 'static, T: Send + 'static> {
    shared: Arc<Shared<K, T>>,
    rx: Receiver<Completed<K, T>>,
    /// The cancel flag of each key's in-flight or pending job. One job per key: a
    /// second submission for the same key supersedes the first, which is what a
    /// user toggling a decode option twice in a second means.
    flags: HashMap<K, Arc<AtomicBool>>,
    workers: Vec<std::thread::JoinHandle<()>>,
    next_seq: u64,
}

impl<K: Copy + Eq + Hash + Send + 'static, T: Send + 'static> Queue<K, T> {
    pub fn new(workers: usize) -> Self {
        let shared = Arc::new(Shared {
            pending: Mutex::new(Vec::new()),
            signal: Condvar::new(),
            stop: AtomicBool::new(false),
            live: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        });
        let (tx, rx) = channel();
        let workers = (0..workers.max(1))
            .map(|i| {
                let shared = Arc::clone(&shared);
                let tx = tx.clone();
                std::thread::Builder::new()
                    .name(format!("decode-{i}"))
                    .spawn(move || worker(shared, tx))
                    .expect("spawn decode worker")
            })
            .collect();
        Self {
            shared,
            rx,
            flags: HashMap::new(),
            workers,
            next_seq: 0,
        }
    }

    /// Queue work for `key`, superseding anything already queued for it.
    pub fn submit(&mut self, key: K, priority: u32, work: impl FnOnce() -> T + Send + 'static) {
        self.cancel(key);
        let cancel = Arc::new(AtomicBool::new(false));
        self.flags.insert(key, Arc::clone(&cancel));
        let seq = self.next_seq;
        self.next_seq += 1;
        {
            let mut q = lock(&self.shared.pending);
            q.push(Job {
                key,
                priority,
                seq,
                cancel,
                work: Box::new(work),
            });
        }
        self.shared.signal.notify_one();
    }

    /// Abandon `key`'s work. A pending job is dropped without running; a running one
    /// finishes but its result is discarded.
    ///
    /// Running jobs finish their current work; polling also rejects completions
    /// that were already sent before cancellation.
    pub fn cancel(&mut self, key: K) {
        if let Some(f) = self.flags.remove(&key) {
            f.store(true, Ordering::Release);
        }
        // Wake a worker so it can retire the cancelled job from the pending list
        // rather than leaving it to be swept by the next submission.
        self.shared.signal.notify_all();
    }

    /// Move `key`'s pending job to the front. What a tab focus change wants: the
    /// ordering was right when it was submitted and is wrong now.
    pub fn promote(&mut self, key: K) {
        let mut q = lock(&self.shared.pending);
        for job in q.iter_mut().filter(|j| j.key == key) {
            job.priority = FOREGROUND;
        }
    }

    /// Take a completion, including a reported job panic. Non-blocking.
    pub fn poll(&mut self) -> Option<(K, Result<T, String>)> {
        loop {
            let Completed { key, cancel, out } = self.rx.try_recv().ok()?;
            // The flag's identity distinguishes submissions even after a result
            // has reached the channel and a replacement uses the same key.
            if !cancel.load(Ordering::Acquire)
                && self
                    .flags
                    .get(&key)
                    .is_some_and(|f| Arc::ptr_eq(f, &cancel))
            {
                self.flags.remove(&key);
                return Some((key, out));
            }
        }
    }

    /// Jobs neither finished nor abandoned.
    pub fn in_flight(&self) -> usize {
        self.flags.len()
    }

    pub fn is_busy(&self, key: K) -> bool {
        self.flags.contains_key(&key)
    }

    /// Most jobs that have ever run at once. For the test that proves the bound.
    #[cfg(test)]
    pub fn peak_concurrency(&self) -> usize {
        self.shared.peak.load(Ordering::Acquire)
    }
}

impl<K: Copy + Eq + Hash + Send + 'static, T: Send + 'static> Drop for Queue<K, T> {
    /// # `stop` is set **under the lock**, and that is the whole of it
    ///
    /// It was set outside, and that is a lost wakeup with a deadlock behind it.
    ///
    /// A worker takes `pending`, checks `stop`, and calls `signal.wait(q)` — which
    /// releases the lock and sleeps *atomically*, but only from the moment it is
    /// called. Between the check and that call the worker still holds the lock and is
    /// not yet waiting. `stop` was an `AtomicBool`, so a dropping thread needed no
    /// lock to set it: it could store `true` and `notify_all` entirely inside that
    /// window, waking nobody, and the worker would then sleep on a condvar that
    /// nothing would ever signal again. `join` waits for it forever.
    ///
    /// Taking the lock closes the window. The dropper can only set `stop` when no
    /// worker is between its check and its wait, so every worker either sees `stop`
    /// on its next check or is already parked and gets the notify.
    ///
    /// **`submit` never had this** and the difference says why: it puts the job into
    /// the queue *under* the lock, so the predicate a worker tests is already true
    /// before the notify is sent. Signalling through shared state that the predicate
    /// does not live in is the mistake, not signalling without the lock.
    ///
    /// Found from a test run that stalled past ten minutes and was nearly written off
    /// as machine load — 800 tests build and drop queues thousands of times, which is
    /// what it takes to hit a window this narrow. The same stall would be an app that
    /// will not quit.
    fn drop(&mut self) {
        {
            let _held = lock(&self.shared.pending);
            self.shared.stop.store(true, Ordering::Release);
        }
        self.shared.signal.notify_all();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

/// Recover access to queue bookkeeping; job execution itself never holds this lock.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn worker<K: Copy + Eq + Hash + Send + 'static, T: Send + 'static>(
    shared: Arc<Shared<K, T>>,
    tx: Sender<Completed<K, T>>,
) {
    loop {
        let job = {
            let mut q = lock(&shared.pending);
            loop {
                if shared.stop.load(Ordering::Acquire) {
                    return;
                }
                // Retire cancelled work without running it — the cheapest
                // cancellation there is, and the one Lightbox's cancel-on-scroll
                // will live on.
                q.retain(|j| !j.cancel.load(Ordering::Acquire));
                // Lowest priority first, then oldest. `remove` rather than
                // `swap_remove`: the list is a handful of entries and preserving
                // order is what makes equal priorities FIFO.
                let best = q
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, j)| (j.priority, j.seq))
                    .map(|(i, _)| i);
                if let Some(i) = best {
                    break q.remove(i);
                }
                q = shared.signal.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };

        let live = shared.live.fetch_add(1, Ordering::AcqRel) + 1;
        shared.peak.fetch_max(live, Ordering::AcqRel);
        // Work owns its inputs and runs outside the queue lock. An unwind must
        // retire this submission without retiring the worker or its capacity.
        let out =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(job.work)).map_err(|payload| {
                let message = payload
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| payload.downcast_ref::<&str>().copied())
                    .unwrap_or("unknown panic payload");
                format!("background job panicked: {message}")
            });
        shared.live.fetch_sub(1, Ordering::AcqRel);

        // Cancelled while we worked: the tab was closed, or a newer request
        // superseded this one. Dropping the result is the point of checking.
        if job.cancel.load(Ordering::Acquire) {
            continue;
        }
        if tx
            .send(Completed {
                key: job.key,
                cancel: job.cancel,
                out,
            })
            .is_err()
        {
            return; // the app is gone
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_panicking_job_reports_completion_and_the_worker_runs_the_next_job() {
        let mut q: Queue<u32, u32> = Queue::new(1);
        q.submit(1, BACKGROUND, || panic!("injected decode failure"));
        q.submit(2, BACKGROUND, || 42);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut results = Vec::new();
        while results.len() < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "worker lost after panic"
            );
            if let Some(result) = q.poll() {
                results.push(result);
            } else {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        assert_eq!(
            results[0],
            (
                1,
                Err("background job panicked: injected decode failure".into())
            )
        );
        assert_eq!(results[1], (2, Ok(42)));
        assert_eq!(q.in_flight(), 0);
        assert_eq!(q.shared.live.load(Ordering::Acquire), 0);
        assert_eq!(q.peak_concurrency(), 1);
    }

    #[test]
    fn superseded_panics_cannot_clear_a_replacement_jobs_tracking() {
        let mut q: Queue<u32, u32> = Queue::new(1);
        q.submit(1, BACKGROUND, || panic!("superseded"));
        let release = gate(&mut q);
        q.submit(1, FOREGROUND, || 42);
        assert!(q.poll().is_none());
        assert!(q.is_busy(1));
        q.cancel(1);
        release();
        assert_eq!(drain(&mut q, 1), [GATE]);
        assert!(!q.is_busy(1));
    }

    #[test]
    fn a_queue_always_shuts_down_however_the_timing_falls() {
        // **The lost-wakeup race, run enough times to catch it.** The window is
        // between a worker checking `stop` and parking on the condvar; a queue built
        // and dropped immediately spends almost all its life inside it, which is why
        // the app never showed this and a test suite doing it thousands of times did.
        //
        // Every iteration must finish. If `stop` is set outside the lock this hangs
        // rather than fails, so the assertion is really the clock: a stuck run is a
        // stuck test binary, which is exactly how it was found.
        for _ in 0..2000 {
            let mut q: Queue<u32, u32> = Queue::new(4);
            // Half of them get work, so some workers are running and some are parked
            // when the drop lands.
            q.submit(1, FOREGROUND, || 1);
            drop(q);
        }

        // And with nothing submitted at all, which is the pure "everyone is parked"
        // case.
        for _ in 0..2000 {
            let q: Queue<u32, u32> = Queue::new(2);
            drop(q);
        }
    }

    /// Key of the job used to hold the worker.
    const GATE: u32 = 99;

    /// Occupy the single worker until the returned closure is called.
    ///
    /// Everything submitted while the gate is held is *queued*, which is what makes
    /// the order these tests assert on a property of the queue rather than of who
    /// won a race — a gate that merely blocks, without confirming it is running,
    /// leaves the pool free to pick a later job first and the test passes or fails
    /// by timing.
    // `use<>`: the releaser borrows nothing from `q`, and in edition 2024 an
    // `impl Trait` return captures every in-scope lifetime unless told otherwise —
    // which here would hold `q` borrowed for the rest of the test.
    fn gate(q: &mut Queue<u32, u32>) -> impl FnOnce() + use<> {
        let (release, gated) = channel::<()>();
        let (started, ack) = channel::<()>();
        q.submit(GATE, BACKGROUND, move || {
            let _ = started.send(());
            let _ = gated.recv();
            GATE
        });
        ack.recv().expect("the gate job never started");
        move || {
            let _ = release.send(());
        }
    }

    /// Collect `n` results, failing rather than hanging.
    fn drain(q: &mut Queue<u32, u32>, n: usize) -> Vec<u32> {
        let mut out = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while out.len() < n {
            assert!(
                std::time::Instant::now() < deadline,
                "only {} of {n} results arrived",
                out.len()
            );
            match q.poll() {
                Some((k, _)) => out.push(k),
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        }
        out
    }

    #[test]
    fn concurrency_is_bounded() {
        // The reason the queue exists. Eight files dropped at once used to mean
        // eight live decodes, each holding a sensor and a scene — ~600 MB apiece on
        // a 100 MP frame.
        let mut q: Queue<u32, u32> = Queue::new(2);
        for i in 0..8 {
            q.submit(i, BACKGROUND, move || {
                std::thread::sleep(Duration::from_millis(20));
                i
            });
        }
        let mut got = 0;
        while got < 8 {
            if q.poll().is_some() {
                got += 1;
            } else {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        assert!(
            q.peak_concurrency() <= 2,
            "ran {} at once",
            q.peak_concurrency()
        );
        assert!(q.peak_concurrency() >= 2, "never used the second worker");
    }

    #[test]
    fn the_foreground_tab_jumps_the_queue() {
        // What the priority is for: the user switches to a tab whose decode was
        // queued behind seven others, and should not wait for them.
        let mut q: Queue<u32, u32> = Queue::new(1);
        let release = gate(&mut q);
        for i in 0..3 {
            q.submit(i, BACKGROUND, move || i);
        }
        q.submit(7, FOREGROUND, || 7);
        release();

        let order = drain(&mut q, 5);
        assert_eq!(order[0], GATE, "the gate should finish first");
        assert_eq!(
            order[1], 7,
            "the foreground tab waited behind background work"
        );
    }

    #[test]
    fn promoting_moves_a_queued_job_to_the_front() {
        // A tab focused *after* its decode was queued. `promote` is what the focus
        // change calls, and without it the ordering stays as it was submitted.
        let mut q: Queue<u32, u32> = Queue::new(1);
        let release = gate(&mut q);
        for i in 0..4 {
            q.submit(i, BACKGROUND, move || i);
        }
        q.promote(3);
        release();

        let order = drain(&mut q, 5);
        assert_eq!(
            order[1], 3,
            "promotion did not reorder the queue: {order:?}"
        );
    }

    #[test]
    fn equal_priorities_stay_in_order() {
        let mut q: Queue<u32, u32> = Queue::new(1);
        let release = gate(&mut q);
        for i in 0..4 {
            q.submit(i, BACKGROUND, move || i);
        }
        release();
        assert_eq!(drain(&mut q, 5), vec![GATE, 0, 1, 2, 3]);
    }

    #[test]
    fn a_closed_tabs_decode_is_dropped_rather_than_delivered() {
        // Tab close cancels. The result must not arrive, because by then there is
        // no tab to give it to and `TabId` is not reused.
        let mut q: Queue<u32, u32> = Queue::new(1);
        let release = gate(&mut q);
        q.submit(1, BACKGROUND, || 1);
        q.submit(2, BACKGROUND, || 2);
        q.cancel(1);
        release();

        // Job 2 is the marker: once it has come back, job 1 has had its turn.
        assert_eq!(
            drain(&mut q, 2),
            vec![GATE, 2],
            "a cancelled job delivered its result"
        );
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            q.poll().is_none(),
            "a cancelled job delivered its result late"
        );
    }

    #[test]
    fn resubmitting_supersedes() {
        // Dragging a decode toggle twice must not run two decodes and then apply
        // the older one's result.
        let mut q: Queue<u32, u32> = Queue::new(1);
        let release = gate(&mut q);
        q.submit(1, BACKGROUND, || 10);
        q.submit(1, BACKGROUND, || 20);
        q.submit(2, BACKGROUND, || 2);
        release();

        let mut values = Vec::new();
        for _ in 0..3 {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                assert!(
                    std::time::Instant::now() < deadline,
                    "results never arrived"
                );
                if let Some((k, v)) = q.poll() {
                    if k == 1 {
                        values.push(v.unwrap());
                    }
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        assert_eq!(values, vec![20], "the superseded job was not dropped");
    }

    #[test]
    fn replacing_a_completed_job_keeps_the_new_job_cancellable() {
        let mut q: Queue<u32, u32> = Queue::new(1);
        q.submit(1, FOREGROUND, || 10);
        // The gate starts only after the first completion has been sent.
        let release = gate(&mut q);
        q.submit(1, FOREGROUND, || 20);
        assert!(q.poll().is_none(), "the old completion was delivered");
        assert!(q.is_busy(1), "the replacement lost its tracking");
        q.cancel(1);
        release();
        assert_eq!(drain(&mut q, 1), vec![GATE]);
        assert!(q.poll().is_none());
    }

    #[test]
    fn cancelling_a_completed_job_discards_its_unread_result() {
        let mut q: Queue<u32, u32> = Queue::new(1);
        q.submit(1, FOREGROUND, || 10);
        let release = gate(&mut q);
        q.cancel(1);
        assert!(q.poll().is_none(), "a cancelled completion was delivered");
        release();
        assert_eq!(drain(&mut q, 1), vec![GATE]);
    }

    #[test]
    fn a_byte_identical_copy_has_the_same_key() {
        // The property the cache rests on: saving a duplicate copies the raw, and
        // the copy must share the original's decode rather than repeat it. A
        // path-keyed or mtime-keyed cache cannot see that.
        let dir = std::env::temp_dir().join(format!("monopro-ck-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let a = dir.join("a.raw");
        let b = dir.join("b.raw");
        // Larger than three windows, so all three probe points land on real bytes.
        let bytes: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&a, &bytes).expect("write a");
        std::fs::write(&b, &bytes).expect("write b");

        let ka = ContentKey::of(&a).expect("key a");
        let kb = ContentKey::of(&b).expect("key b");
        assert_eq!(ka, kb, "a byte-identical copy must key the same");

        // And a different file must not collide. The change is in the middle, which
        // is what the mid window is for.
        let mut other = bytes.clone();
        other[150_000] ^= 0xff;
        let c = dir.join("c.raw");
        std::fs::write(&c, &other).expect("write c");
        assert_ne!(
            ka,
            ContentKey::of(&c).expect("key c"),
            "distinct files collided"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_cache_does_not_keep_a_closed_tabs_scene_alive() {
        // Weak entries are the whole design: the memory story is N *distinct* open
        // images, so the cache must share without extending a lifetime.
        let key = ContentKey { len: 1, sample: 2 };
        let opts = DecodeOptions::default();
        let mut cache = Cache::default();

        let geom = raw_core::CfaGeometry::new(
            2,
            raw_core::Dims { w: 2, h: 2 },
            0,
            0,
            2,
            2,
            [
                [
                    raw_core::geometry::CfaColor::Green,
                    raw_core::geometry::CfaColor::Blue,
                ],
                [
                    raw_core::geometry::CfaColor::Red,
                    raw_core::geometry::CfaColor::Green,
                ],
            ],
        );
        let decoded = Arc::new(Decoded {
            opts,
            scene: SceneImage {
                data: vec![0.0; 4],
                geom,
                gains: raw_core::sensor::Gains([1.0, 1.0, 1.0]),
                camera: "test".into(),
                clipped: Vec::new(),
            },
            clipped_fraction: 0.0,
            decode_ms: 0,
        });
        cache.put_decoded(key, &decoded);
        assert!(
            cache.decoded(key, opts).is_some(),
            "a live scene must be shared"
        );

        drop(decoded);
        assert!(
            cache.decoded(key, opts).is_none(),
            "the cache outlived the last tab"
        );
        cache.sweep();
        assert_eq!(cache.len(), (0, 0));
    }

    #[test]
    fn a_reflinked_copy_shares_the_originals_decode() {
        // The claim saving a duplicate rests on, checked against the real
        // filesystem rather than assumed: a copy-on-write copy must be
        // byte-identical, so its ContentKey matches and the decode cache hands it
        // the SceneImage already in memory instead of decoding the same bytes
        // twice.
        let dir = std::env::temp_dir().join(format!("monopro-rf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let a = dir.join("orig.raw");
        let b = dir.join("orig_dup1.raw");
        let bytes: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&a, &bytes).expect("write");

        reflink_copy::reflink_or_copy(&a, &b).expect("reflink or copy");
        assert_eq!(
            ContentKey::of(&a).expect("key a"),
            ContentKey::of(&b).expect("key b"),
            "the copy is not byte-identical, so a duplicate would decode twice"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
