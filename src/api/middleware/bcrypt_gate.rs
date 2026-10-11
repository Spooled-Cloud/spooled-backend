//! Bounded bcrypt verification for API keys.
//!
//! Keys are hashed at bcrypt cost 12, so one `bcrypt::verify` is ~250 ms of
//! pure CPU. Called inline in an async fn it parked a tokio worker thread for
//! that long: a burst of cache misses (every unary gRPC call, before it shared
//! the REST cache) or of bogus tokens hitting the legacy scan stalled every
//! other request, the scheduler and the health probes on the same runtime.
//!
//! Every verify now runs on the blocking pool behind one small process-wide
//! semaphore, so bcrypt can occupy a few cores at most however many requests
//! ask for it. Legacy-key scans additionally take turns, one at a time per
//! transport ([`ScanLane`]), and wait for a turn only as long as a scan is
//! expected to take ([`legacy_scan_turn`]).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use tokio::sync::{Semaphore, SemaphorePermit, TryAcquireError};

/// Concurrent verifications allowed in this process.
static VERIFY_PERMITS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(verify_concurrency())));

/// Bcrypt cost API keys are hashed with (`bcrypt::DEFAULT_COST`). A hash whose
/// cost cannot be read is estimated at it.
const KEY_COST: u32 = 12;

/// What one verify at [`KEY_COST`] is assumed to take until verifies on this
/// host have been measured (~220-250 ms on the production hosts).
const SEED_KEY_VERIFY: Duration = Duration::from_millis(250);

/// Measured nanoseconds per unit of bcrypt work, a unit being one of the
/// `2^cost` key-setup rounds (see [`verify_units`]). A moving average of
/// every verify, seeded from [`SEED_KEY_VERIFY`].
static NANOS_PER_UNIT: AtomicU64 = AtomicU64::new((SEED_KEY_VERIFY.as_nanos() >> KEY_COST) as u64);

/// Added to the expected scan time a request waits for its lane's turn:
/// the scan's queries, verifies of keys found by `lookup_hash` that share the
/// verify permits, and scheduling.
const LEGACY_SCAN_WAIT_MARGIN: Duration = Duration::from_secs(1);

/// Longest a request waits for its lane's turn, however large the scans: well
/// under the 30 s request timeout, leaving room for its own scan.
pub(crate) const LEGACY_SCAN_MAX_WAIT: Duration = Duration::from_secs(10);

/// Half the cores, between 1 and 4.
fn verify_concurrency() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    (cores / 2).clamp(1, 4)
}

/// Check `token` against a stored bcrypt hash without blocking an async worker.
///
/// A malformed hash is a mismatch, as before. `Err` means the check could not
/// run at all; callers must not read that as "no match", or a valid key could
/// be negatively cached.
pub(crate) async fn verify(token: &str, hash: &str) -> anyhow::Result<bool> {
    let token = token.to_owned();
    let hash = hash.to_owned();
    run_bounded(move || {
        let started = Instant::now();
        match bcrypt::verify(token, &hash) {
            Ok(matched) => {
                record_verify(verify_units(&hash), started.elapsed());
                matched
            }
            Err(_) => false,
        }
    })
    .await
}

/// Run `work` on the blocking pool once a verify permit is free.
///
/// The permit moves into the blocking task, so a caller that is cancelled
/// mid-verify (client gone, request timeout) does not free its slot while the
/// hash is still being computed.
async fn run_bounded<R, F>(work: F) -> anyhow::Result<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let permit = Arc::clone(&VERIFY_PERMITS).acquire_owned().await?;
    Ok(tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await?)
}

/// Work of one verify against `hash`, in units of one bcrypt key-setup round:
/// `2^cost`, read from the hash (`$2b$12$...`).
pub(crate) fn verify_units(hash: &str) -> u64 {
    let cost = hash
        .split('$')
        .nth(2)
        .and_then(|cost| cost.parse::<u32>().ok())
        .filter(|cost| (4..=31).contains(cost))
        .unwrap_or(KEY_COST);
    1u64 << cost
}

/// Fold one measured verify into [`NANOS_PER_UNIT`].
fn record_verify(units: u64, took: Duration) {
    let sample = u64::try_from(took.as_nanos() / u128::from(units.max(1))).unwrap_or(u64::MAX);
    let previous = NANOS_PER_UNIT.load(Ordering::Relaxed);
    NANOS_PER_UNIT.store(previous / 2 + sample / 2, Ordering::Relaxed);
}

/// Expected time to verify `units` of bcrypt work on this host.
fn estimate(units: u64) -> Duration {
    Duration::from_nanos(units.saturating_mul(NANOS_PER_UNIT.load(Ordering::Relaxed)))
}

/// The transport a legacy scan runs for. Each has its own turn, so unknown
/// tokens on one can only delay legacy keys on that one: a gRPC flood (one
/// failed-auth budget for every gRPC caller) used to hold the single
/// process-wide turn that REST legacy-key holders needed (W3R-06, W3R-08).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ScanLane {
    /// REST and `/auth/login`.
    Rest,
    /// gRPC.
    Grpc,
}

/// One lane's turn and what the scan holding it costs.
struct Lane {
    turn: Semaphore,
    /// Work units of the scan holding the turn; 0 while it is free.
    holder_units: AtomicU64,
}

impl Lane {
    const fn new() -> Self {
        Self {
            turn: Semaphore::const_new(1),
            holder_units: AtomicU64::new(0),
        }
    }
}

static REST_LANE: Lane = Lane::new();
static GRPC_LANE: Lane = Lane::new();

impl ScanLane {
    fn lane(self) -> &'static Lane {
        match self {
            Self::Rest => &REST_LANE,
            Self::Grpc => &GRPC_LANE,
        }
    }

    fn other(self) -> Self {
        match self {
            Self::Rest => Self::Grpc,
            Self::Grpc => Self::Rest,
        }
    }
}

/// A lane's turn to bcrypt-scan legacy keys; the turn ends when this drops.
pub(crate) struct ScanTurn {
    _permit: SemaphorePermit<'static>,
    lane: &'static Lane,
    waited: bool,
}

impl ScanTurn {
    /// Whether another scan held the turn when this request asked for it. A
    /// request that waited re-checks the token before scanning: the scan it
    /// waited for may have resolved it.
    pub(crate) fn waited(&self) -> bool {
        self.waited
    }
}

impl Drop for ScanTurn {
    fn drop(&mut self) {
        self.lane.holder_units.store(0, Ordering::Relaxed);
    }
}

/// How long a scan of `units` waits for `lane`'s turn: the larger of its own
/// scan and the one holding the turn, at the measured verify speed, plus
/// [`LEGACY_SCAN_WAIT_MARGIN`], at most [`LEGACY_SCAN_MAX_WAIT`].
///
/// With a single verify permit, a scan on the other lane takes every other
/// verify, so the expected time doubles while one runs.
fn scan_wait(lane: ScanLane, units: u64) -> Duration {
    let holder = lane.lane().holder_units.load(Ordering::Relaxed);
    let mut expected = estimate(units.max(holder));
    if verify_concurrency() < 2 && lane.other().lane().holder_units.load(Ordering::Relaxed) > 0 {
        expected = expected.saturating_mul(2);
    }
    expected
        .saturating_add(LEGACY_SCAN_WAIT_MARGIN)
        .min(LEGACY_SCAN_MAX_WAIT)
}

/// Wait for `lane`'s turn to bcrypt-scan legacy (`lookup_hash IS NULL`) keys,
/// for a scan of `units` of work (the sum of [`verify_units`] over its
/// candidates). `Ok(None)` means the turn stayed busy for the bounded wait
/// ([`scan_wait`]).
///
/// Any unknown token can trigger a scan, and a scan costs one verify per legacy
/// row. Running one per lane at a time keeps at most two scan verifies queued
/// for a permit, so a key found by `lookup_hash` (a single verify) never waits
/// behind a flood of scans. The wait covers one whole scan (the flat 2 s it
/// replaced was shorter than a scan of ten production keys, so a request for a
/// legacy key gave up behind another request's scan, W3R-07) and is bounded so
/// a flood of unknown tokens cannot hold a legacy key's request until the
/// request timeout.
pub(crate) async fn legacy_scan_turn(
    lane: ScanLane,
    units: u64,
) -> anyhow::Result<Option<ScanTurn>> {
    let state = lane.lane();
    let (permit, waited) = match state.turn.try_acquire() {
        Ok(permit) => (permit, false),
        Err(TryAcquireError::NoPermits) => {
            match acquire_within(&state.turn, scan_wait(lane, units)).await? {
                Some(permit) => (permit, true),
                None => return Ok(None),
            }
        }
        Err(TryAcquireError::Closed) => anyhow::bail!("the legacy scan turn is closed"),
    };
    state.holder_units.store(units, Ordering::Relaxed);
    Ok(Some(ScanTurn {
        _permit: permit,
        lane: state,
        waited,
    }))
}

/// A permit of `semaphore`, or `None` if none came free within `wait`.
async fn acquire_within(
    semaphore: &Semaphore,
    wait: Duration,
) -> anyhow::Result<Option<SemaphorePermit<'_>>> {
    match tokio::time::timeout(wait, semaphore.acquire()).await {
        Ok(permit) => Ok(Some(permit?)),
        Err(_elapsed) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// On a single-threaded runtime, another task keeps running while a verify
    /// is in flight: bcrypt is not on the async worker.
    #[tokio::test(flavor = "current_thread")]
    async fn verify_does_not_block_the_async_worker() {
        let hash = bcrypt::hash("sp_test_gate", 11).unwrap();
        let ticks = Arc::new(AtomicUsize::new(0));
        let ticker = {
            let ticks = Arc::clone(&ticks);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    ticks.fetch_add(1, Ordering::Relaxed);
                }
            })
        };
        tokio::task::yield_now().await;

        let before = ticks.load(Ordering::Relaxed);
        assert!(verify("sp_test_gate", &hash).await.unwrap());
        let during = ticks.load(Ordering::Relaxed) - before;
        ticker.abort();
        assert!(during > 0, "the runtime was blocked for the whole verify");

        assert!(!verify("sp_test_other", &hash).await.unwrap());
        assert!(!verify("sp_test_gate", "not-a-bcrypt-hash").await.unwrap());
    }

    // A request waits a bounded time for a busy scan, then gives up instead
    // of queueing behind every unknown token in the process.
    #[tokio::test]
    async fn a_busy_scan_is_waited_for_a_bounded_time() {
        let semaphore = Semaphore::new(1);
        let held = acquire_within(&semaphore, Duration::from_millis(10))
            .await
            .unwrap()
            .expect("a free permit is granted at once");

        let started = tokio::time::Instant::now();
        let waited = acquire_within(&semaphore, Duration::from_millis(200))
            .await
            .unwrap();
        assert!(waited.is_none(), "a held permit must not be granted");
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(200) && elapsed < Duration::from_secs(2),
            "gave up after {elapsed:?}"
        );

        drop(held);
        assert!(acquire_within(&semaphore, Duration::from_millis(200))
            .await
            .unwrap()
            .is_some());
    }

    #[test]
    fn verify_units_follow_the_hash_cost() {
        assert_eq!(verify_units(&bcrypt::hash("k", 4).unwrap()), 1 << 4);
        assert_eq!(verify_units("$2b$12$abcdefghijklmnopqrstuv"), 1 << 12);
        assert_eq!(verify_units("$2y$10$abcdefghijklmnopqrstuv"), 1 << 10);
        // Unreadable: estimated like a key hash.
        assert_eq!(verify_units("not-a-bcrypt-hash"), 1 << KEY_COST);
        assert_eq!(verify_units("$2b$99$abc"), 1 << KEY_COST);
    }

    // W3R-07: the wait for a lane's turn covers a whole scan at the measured
    // verify speed (a flat 2 s was shorter than a scan of ten production
    // keys), and stays bounded however large the scans.
    #[test]
    fn the_turn_wait_covers_a_scan_and_is_capped() {
        let key = 1u64 << KEY_COST;
        let one = estimate(key);
        assert!(
            one >= Duration::from_millis(100) && one <= Duration::from_secs(1),
            "seeded estimate of one cost-12 verify: {one:?}"
        );

        // Ten production keys: about 2.5 s of verifies, all waited for.
        let ten = scan_wait(ScanLane::Rest, 10 * key);
        assert!(
            ten >= estimate(10 * key) + LEGACY_SCAN_WAIT_MARGIN,
            "{ten:?}"
        );
        assert!(ten > Duration::from_secs(2), "{ten:?}");
        // Bounded however many rows.
        assert_eq!(scan_wait(ScanLane::Rest, 1_000 * key), LEGACY_SCAN_MAX_WAIT);
        assert!(LEGACY_SCAN_MAX_WAIT <= Duration::from_secs(15));

        // The estimate follows measured verifies.
        record_verify(key, Duration::from_millis(400));
        record_verify(key, Duration::from_millis(400));
        record_verify(key, Duration::from_millis(400));
        let measured = estimate(key);
        assert!(
            measured > Duration::from_millis(350) && measured <= Duration::from_millis(400),
            "{measured:?}"
        );
    }

    // W3R-08: REST and gRPC scans take separate turns, and a waiter covers
    // the scan holding its lane's turn, not only its own.
    #[tokio::test]
    async fn lanes_take_separate_turns() {
        let key = 1u64 << KEY_COST;
        let grpc = legacy_scan_turn(ScanLane::Grpc, 20 * key)
            .await
            .unwrap()
            .expect("a free lane");
        assert!(!grpc.waited());

        let started = Instant::now();
        let rest = legacy_scan_turn(ScanLane::Rest, key)
            .await
            .unwrap()
            .expect("a gRPC scan must not hold the REST turn");
        assert!(!rest.waited());
        assert!(started.elapsed() < Duration::from_millis(100));

        // A one-row gRPC scan behind a twenty-row one waits for the twenty rows.
        assert!(scan_wait(ScanLane::Grpc, key) >= estimate(20 * key));
        drop(grpc);
        assert_eq!(GRPC_LANE.holder_units.load(Ordering::Relaxed), 0);
        let next = legacy_scan_turn(ScanLane::Grpc, key)
            .await
            .unwrap()
            .expect("the turn is free again");
        assert!(!next.waited());
        drop(rest);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_verifies_never_exceed_the_permits() {
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let tasks: Vec<_> = (0..16)
            .map(|_| {
                let running = Arc::clone(&running);
                let peak = Arc::clone(&peak);
                tokio::spawn(run_bounded(move || {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(20));
                    running.fetch_sub(1, Ordering::SeqCst);
                }))
            })
            .collect();
        for task in tasks {
            task.await.unwrap().unwrap();
        }

        let peak = peak.load(Ordering::SeqCst);
        assert!(
            (1..=verify_concurrency()).contains(&peak),
            "peak {peak} concurrent verifies, limit {}",
            verify_concurrency()
        );
    }
}
