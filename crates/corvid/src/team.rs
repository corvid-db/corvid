//! A scoped team of worker threads for one parallel region — std-only
//! fork/join for the engine's deterministic batch loops (roadmap Task 13:
//! parallel PQ training).
//!
//! The design constraints, and how they shape this module:
//!
//! * **No new dependencies.** rayon would either join the default build
//!   (changing the wasm/bundle posture) or hide behind a non-default
//!   feature — in which case the default build would not be parallel at
//!   all and the acceptance bench would not move. A hand-rolled
//!   `std::thread::scope` team gives the same fork/join shape with zero
//!   dependency-graph impact (and no MSRV change).
//!
//! * **The team is scoped to one parallel region, not global.** Workers
//!   are spawned when a parallel phase starts (`Pq::train`) and joined
//!   when it ends, via [`with_team`]. A global lazily-spawned pool would
//!   have to accept `'static`-borrowing jobs; a scoped team's jobs are
//!   `Arc`-owned closures over data that merely needs to outlive the
//!   `fork` call. Two concurrent regions each own their team — no
//!   cross-region contention.
//!
//! * **Map with thread-disjoint outputs, never locks on the hot path.**
//!   [`Team::map`] evaluates `f(i)` for every item `i < n`, splitting the
//!   range into contiguous chunks: chunk 0 on the calling thread, chunk
//!   `w + 1` on worker `w`. Each participant computes its chunk into its
//!   own buffer and only then moves the values into the shared slots
//!   under one short lock — no computation happens under any lock, and
//!   each output slot is written by exactly one participant. Callers get
//!   item-indexed outputs in input order: the deterministic-reduction
//!   shape the build paths require (same values as the sequential loop,
//!   computed by the same pure functions).
//!
//! * **Cheap dispatch.** Workers spin briefly watching the sequence
//!   counter before parking on the condvar, so back-to-back batches
//!   inside one region (k-means iterations) stay hot, while an idle
//!   team's workers sleep in the OS and cost nothing.
//!
//! * **Panics poison, they never hang.** A panic inside a dispatched job
//!   or the region callback would otherwise strand the handshake (a
//!   worker that unwinds skips its completion count; a forking thread
//!   that unwinds skips the shutdown bump and `thread::scope` never
//!   joins). Jobs run under [`std::panic::catch_unwind`] and the region
//!   callback is wrapped the same way: the completion/shutdown handshake
//!   always runs to completion, then the panic is re-raised on the
//!   forking thread (the original payload, via
//!   [`std::panic::resume_unwind`]). A panicking region therefore fails
//!   promptly, loudly, and with every worker joined.

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

/// The published dispatch: a chunk evaluator, the item count, and the
/// participant count it was chunked for. Workers derive their own chunk
/// bounds from these, so no shared chunk state exists. Cloned (an `Arc`
/// bump), never taken — the forking thread clears the slot after the
/// join, which is also why a worker must re-check `seq` before serving.
#[derive(Clone)]
struct Dispatch {
    /// Called as `f(start, end)` over one participant's contiguous chunk.
    f: Arc<dyn Fn(usize, usize) + Send + Sync>,
    n: usize,
    participants: usize,
}

/// Shared state between the forking thread and the workers.
struct Inner {
    /// The in-flight dispatch, installed just before `seq` is bumped.
    /// A `seq` bump with the slot left empty is the shutdown signal.
    slot: Mutex<Option<Dispatch>>,
    /// Bumped under `park_lock` once per dispatch and once at shutdown;
    /// workers remember the last value they served.
    seq: AtomicU64,
    /// Completed chunks for the in-flight dispatch (`participants - 1`
    /// when complete). Reset by the forking thread before publishing.
    done: AtomicUsize,
    /// Whether the forking thread is (or is about to be) parked waiting
    /// for `done` — workers check it before doing a completion wakeup.
    caller_waiting: AtomicBool,
    /// Pairs `seq` bumps with condvar notifies (publish and shutdown);
    /// also the workers' park.
    park_lock: Mutex<()>,
    park_cv: Condvar,
    /// Wakes the forking thread once `done` reaches the worker count.
    done_lock: Mutex<()>,
    done_cv: Condvar,
    /// Workers increment this on entering their service loop; the forking
    /// thread waits for it before the first dispatch. Without the barrier
    /// a worker could snapshot `seq` after the first publish and skip
    /// that dispatch (and its completion count) — a deadlock.
    ready: AtomicUsize,
    /// Poison flag: set by a worker whose dispatched chunk panicked.
    /// Once set, `map` re-raises the stashed payload after the join —
    /// results from a region that lost a job are never returned. The
    /// handshake itself always completes first. (A panic in the forking
    /// thread's own chunk needs no flag: it unwinds through `with_team`,
    /// which runs the shutdown handshake before re-raising.)
    poisoned: AtomicBool,
    /// The first panicked chunk's payload, stashed for the forking thread
    /// to `resume_unwind` (later panics in the same dispatch are dropped:
    /// one panic is re-raised, faithfully).
    panic_payload: Mutex<Option<Box<dyn Any + Send>>>,
}

/// Contiguous chunk `idx` of `n` items over `participants` chunks.
fn chunk_of(idx: usize, n: usize, participants: usize) -> (usize, usize) {
    let chunk = n.div_ceil(participants);
    let start = idx * chunk;
    (start, (start + chunk).min(n))
}

/// Spin iterations a worker burns watching for the next dispatch before
/// parking. Sized to cover the short sequential phases between a region's
/// dispatches (k-means' centroid-update steps) so a team stays hot, while
/// genuinely idle workers reach the park and stop costing anything. The
/// park path is correct regardless (a publish re-checks under the lock
/// before waiting) — this is a latency/CPU knob, not a correctness one.
const WORKER_SPIN: usize = 30_000;

/// Spin iterations the forking thread burns waiting for completion before
/// parking. A batch's parallel phase is short; only an OS preemption of a
/// straggler worker reaches the park path.
const CALLER_SPIN: usize = 200_000;

/// Hard floor on dispatched items: below this the handshake cannot pay
/// for itself, whoever asks.
const FLOOR_MIN_ITEMS: usize = 256;

/// How many threads should participate in a parallel phase:
/// `available_parallelism` capped at 8.
pub(crate) fn parallelism() -> usize {
    thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 8)
}

/// The ergonomic front door to [`Team::map`]: evaluate `f(i)` for every
/// `i < n` and collect the results in order. See `map` for the purity
/// contract (it is what makes every caller deterministic).
pub(crate) fn map_owned<T, F>(team: &mut Team, n: usize, f: F) -> Vec<T>
where
    T: Send + 'static,
    F: Fn(usize) -> T + Send + Sync + 'static,
{
    team.map(n, Arc::new(f))
}

/// Run `f` with a [`Team`] of `participants` threads (the caller
/// included). Workers are spawned for the duration of `f` and joined
/// before it returns; `f` receives a team with `participants - 1`
/// workers.
///
/// `participants <= 1` — or a spawn failure (a thread-restricted
/// environment) — still runs `f`, with a workerless team whose every
/// `map` degenerates to a sequential loop. Parallel users therefore stay
/// correct without the team: the fallback is the same code with fewer
/// helpers.
pub(crate) fn with_team<R>(participants: usize, f: impl FnOnce(&mut Team) -> R) -> R {
    let inner = Arc::new(Inner {
        slot: Mutex::new(None),
        seq: AtomicU64::new(0),
        done: AtomicUsize::new(0),
        caller_waiting: AtomicBool::new(false),
        park_lock: Mutex::new(()),
        park_cv: Condvar::new(),
        done_lock: Mutex::new(()),
        done_cv: Condvar::new(),
        ready: AtomicUsize::new(0),
        poisoned: AtomicBool::new(false),
        panic_payload: Mutex::new(None),
    });
    let shutdown = Arc::clone(&inner);
    let mut spawned = 0usize;
    thread::scope(|scope| {
        for w in 0..participants.saturating_sub(1) {
            let inner = Arc::clone(&inner);
            let spawn = thread::Builder::new()
                .name(format!("corvid-team-{w}"))
                .spawn_scoped(scope, move || worker_loop(&inner, w));
            if spawn.is_ok() {
                spawned += 1;
            }
        }
        // Readiness barrier: every worker must have snapshotted `seq`
        // before the first dispatch is published, or a late starter would
        // treat that dispatch as already served and never contribute its
        // completion count (deadlock). Bounded by thread-start latency,
        // paid once per team.
        while inner.ready.load(Ordering::Acquire) < spawned {
            // yield_now (not spin_loop): the wait is bounded by OS
            // thread-start latency, which the scheduler always makes
            // progress on — no spin budget is consumed here.
            thread::yield_now();
        }
        // The region runs under catch_unwind so the shutdown handshake
        // below executes even when it panics (otherwise the workers
        // would re-park forever and `thread::scope` would never join);
        // the panic is re-raised on this thread after the join.
        let result = catch_unwind(AssertUnwindSafe(|| {
            f(&mut Team {
                inner,
                workers: spawned,
                min_items: FLOOR_MIN_ITEMS.max((spawned + 1) * 32),
            })
        }));
        // Shutdown handshake: clear the slot, then bump `seq` under
        // `park_lock`. The clearing matters on the unwind path — a `map`
        // abandoned mid-dispatch leaves its dispatch published, and a
        // worker must not mistake that stale dispatch for new work.
        // Every worker takes one final lap, sees no dispatch, and
        // returns; the scope then joins them.
        if spawned > 0 {
            *shutdown.slot.lock().unwrap() = None;
            let _guard = shutdown.park_lock.lock().unwrap();
            shutdown.seq.fetch_add(1, Ordering::Release);
            shutdown.park_cv.notify_all();
        }
        match result {
            Ok(result) => result,
            Err(payload) => resume_unwind(payload),
        }
    })
}

/// The worker service loop: serve dispatches until shutdown.
fn worker_loop(inner: &Inner, worker: usize) {
    // Announce readiness BEFORE the snapshot: the forking thread waits for
    // all announcements before publishing the first dispatch (see
    // `with_team`), so this load cannot observe a published dispatch.
    inner.ready.fetch_add(1, Ordering::Release);
    let mut served = inner.seq.load(Ordering::Relaxed);
    loop {
        // Hot wait: spin for the next dispatch...
        for _ in 0..WORKER_SPIN {
            if inner.seq.load(Ordering::Relaxed) != served {
                break;
            }
            std::hint::spin_loop();
        }
        if inner.seq.load(Ordering::Relaxed) == served {
            // ...then park. A publish bumps `seq` under `park_lock` before
            // notifying, and we re-check under the same lock, so no
            // dispatch can be missed; the timeout is belt-and-braces.
            let guard = inner.park_lock.lock().unwrap();
            if inner.seq.load(Ordering::Relaxed) == served {
                let (_g, _t) = inner
                    .park_cv
                    .wait_timeout(guard, std::time::Duration::from_millis(50))
                    .unwrap();
                continue;
            }
        }
        let seq = inner.seq.load(Ordering::Acquire);
        if seq == served {
            continue;
        }
        served = seq;
        let dispatch = inner.slot.lock().unwrap().clone();
        let Some(dispatch) = dispatch else {
            return; // shutdown: a seq bump with no dispatch
        };
        let (start, end) = chunk_of(worker + 1, dispatch.n, dispatch.participants);
        if start < end {
            // Poison, don't die: a panicking chunk is caught so this
            // worker still contributes its completion count (the caller's
            // join completes) and the payload is stashed for the caller
            // to re-raise. An unwinding worker could do neither.
            if let Err(payload) = catch_unwind(AssertUnwindSafe(|| (dispatch.f)(start, end))) {
                inner.poisoned.store(true, Ordering::Release);
                let mut stash = inner.panic_payload.lock().unwrap();
                if stash.is_none() {
                    *stash = Some(payload);
                }
            }
        }
        let done = inner.done.fetch_add(1, Ordering::Release) + 1;
        if inner.caller_waiting.load(Ordering::Relaxed)
            || done >= dispatch.participants.saturating_sub(1)
        {
            let _guard = inner.done_lock.lock().unwrap();
            inner.done_cv.notify_all();
        }
    }
}

/// A scoped team of workers for one parallel region (see the module
/// docs). Owned by one forking thread at a time — `map` takes `&mut
/// self`, which is also the compile-time proof that one team never
/// serves two concurrent forking threads.
pub(crate) struct Team {
    inner: Arc<Inner>,
    /// Number of spawned workers (0 = sequential fallback).
    workers: usize,
    /// Items below this are served by the caller alone: a dispatch must
    /// carry enough items for chunk-parallelism to beat the handshake.
    min_items: usize,
}

impl Team {
    /// Number of helper threads in this team (0 = sequential fallback).
    pub(crate) fn workers(&self) -> usize {
        self.workers
    }

    /// The team's item-count floor for dispatching: `map` runs caller-side
    /// alone below it. Exposed so callers can skip setup work (clones,
    /// gathers) that only a dispatch would consume.
    pub(crate) fn min_items(&self) -> usize {
        self.min_items
    }

    /// Evaluate `f(i)` for every `i < n` and collect the results in
    /// order, chunk-parallel across the team (chunk 0 on this thread).
    /// Sequential — one call sequence, same values — when the team has no
    /// workers or `n` is below the dispatch floor.
    ///
    /// `f` must be a pure per-item function: it may read its captures
    /// immutably and must write nothing shared (`map` gathers the
    /// outputs itself). That is what makes every caller deterministic by
    /// construction — the result is exactly `(0..n).map(f)`, however
    /// many threads ran it.
    pub(crate) fn map<T: Send + 'static>(
        &mut self,
        n: usize,
        f: Arc<dyn Fn(usize) -> T + Send + Sync>,
    ) -> Vec<T> {
        let participants = self.workers + 1;
        if self.workers == 0 || n < self.min_items.max(participants) {
            return (0..n).map(|i| f(i)).collect();
        }
        // Per-participant staging: each participant computes its chunk
        // into a private buffer, then moves the values into the shared
        // slots under one short lock — no computation under any lock, one
        // writer per slot.
        let slots: Arc<Mutex<Vec<Option<T>>>> =
            Arc::new(Mutex::new((0..n).map(|_| None).collect()));
        let chunk_fn: Arc<dyn Fn(usize, usize) + Send + Sync> = {
            let slots = Arc::clone(&slots);
            Arc::new(move |start: usize, end: usize| {
                let mut local: Vec<T> = Vec::with_capacity(end - start);
                for i in start..end {
                    local.push(f(i));
                }
                let mut guard = slots.lock().unwrap();
                for (slot, value) in guard[start..end].iter_mut().zip(local) {
                    *slot = Some(value);
                }
            })
        };
        let inner = &self.inner;
        let dispatch = Dispatch {
            f: Arc::clone(&chunk_fn),
            n,
            participants,
        };
        // Publish: reset the completion counter, install the dispatch,
        // then bump `seq` under `park_lock` + notify (a worker either
        // catches the new `seq` while spinning or re-checks under the
        // lock before parking — no lost wakeup).
        inner.done.store(0, Ordering::Relaxed);
        *inner.slot.lock().unwrap() = Some(dispatch);
        {
            let _guard = inner.park_lock.lock().unwrap();
            inner.seq.fetch_add(1, Ordering::Release);
            inner.park_cv.notify_all();
        }
        // Serve chunk 0 ourselves, then wait for the workers' chunks.
        let (start0, end0) = chunk_of(0, n, participants);
        chunk_fn(start0, end0);
        let target = self.workers;
        for _ in 0..CALLER_SPIN {
            if inner.done.load(Ordering::Acquire) >= target {
                break;
            }
            std::hint::spin_loop();
        }
        if inner.done.load(Ordering::Acquire) < target {
            inner.caller_waiting.store(true, Ordering::Relaxed);
            let mut guard = inner.done_lock.lock().unwrap();
            while inner.done.load(Ordering::Acquire) < target {
                guard = inner.done_cv.wait(guard).unwrap();
            }
            inner.caller_waiting.store(false, Ordering::Relaxed);
        }
        *inner.slot.lock().unwrap() = None;
        // Poison check BEFORE collecting: a panicked participant's slots
        // are unfilled (the collect's `expect` would misreport), and a
        // region that lost a job must panic, not return partial results.
        if inner.poisoned.load(Ordering::Acquire) {
            let payload = inner.panic_payload.lock().unwrap().take();
            if let Some(payload) = payload {
                resume_unwind(payload);
            }
            panic!("corvid team: a dispatched job panicked without a payload");
        }
        let mut filled = slots.lock().unwrap();
        filled
            .iter_mut()
            .map(|slot| slot.take().expect("every slot filled by its chunk"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How long a poison probe may take before it counts as a hang. The
    /// handshake costs microseconds; the pre-poison failure mode was a
    /// wait with no bound at all, so a watchdog turns that regression
    /// into a failing test instead of a stalled binary.
    const HANG_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

    /// A panicking dispatched job must poison the team and come back as a
    /// panic on the forking thread — promptly. The probe panics on the
    /// LAST item, which lands in the final worker's chunk, so with
    /// workers this exercises the worker-side catch + completion +
    /// re-raise path; the workerless fallback panics on the caller
    /// directly. Either way the contract is the same one.
    #[test]
    fn panicking_job_poisons_the_team_and_propagates_promptly() {
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                with_team(parallelism(), |team| {
                    let n = team.min_items().max(4096);
                    let _ = map_owned(team, n, move |i| {
                        if i == n - 1 {
                            panic!("team poison probe");
                        }
                        i
                    });
                });
            }));
            let _ = tx.send(outcome);
        });
        let outcome = rx
            .recv_timeout(HANG_LIMIT)
            .expect("poisoned map returned (no hang)");
        match outcome {
            Ok(()) => panic!("a poisoned map must panic, not return"),
            Err(payload) => {
                let msg = payload
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("<non-string payload>");
                assert!(
                    msg.contains("team poison probe"),
                    "wrong panic re-raised: {msg}"
                );
            }
        }
    }

    /// A panicking region callback must not strand the workers: the
    /// shutdown handshake runs on the unwind path, `thread::scope` joins,
    /// and the panic reaches the caller.
    #[test]
    fn panicking_region_callback_still_shuts_down_and_propagates() {
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                with_team(parallelism(), |_| -> () {
                    panic!("region poison probe");
                });
            }));
            let _ = tx.send(outcome);
        });
        let outcome = rx
            .recv_timeout(HANG_LIMIT)
            .expect("panicking region returned (workers joined, no hang)");
        match outcome {
            Ok(()) => panic!("a panicking region must propagate the panic"),
            Err(payload) => {
                let msg = payload
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("<non-string payload>");
                assert!(
                    msg.contains("region poison probe"),
                    "wrong panic re-raised: {msg}"
                );
            }
        }
    }
}
