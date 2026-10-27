//! Waking a sandbox because a request arrived for it.
//!
//! Without this, a frozen or snapshotted sandbox answers its gateway port
//! with a 503 telling the caller to POST `/touch` to the master and try
//! again. That makes every client of an exposed port aware of tiers, and it
//! is the reason the density figures came with an asterisk: a sandbox that
//! has been demoted is only "available" to callers who know to wake it.
//!
//! Here the gateway wakes it instead. A forward that fails with connection
//! refused -- #327 makes a cold sandbox refuse at once rather than hang --
//! wakes the sandbox on this node and forwards again, and the caller sees a
//! slow response rather than an error. Nothing goes through the master,
//! which RFC §15.5 requires of a wake ("唤醒请求绝不经过 master").
//!
//! The master still owns desired state, so two things keep it and the node
//! from fighting over a sandbox that was just woken:
//!
//! - every request is reported back (`PollRequest::traffic`), and the
//!   master moves the idle clock and makes Hot the desired tier;
//! - until it has, the node will not demote a sandbox that served a
//!   request in the last `GRACE`, so a demotion the master issued from a
//!   stale view is deferred rather than undoing the wake.
//!
//! The node is the one that pays for a wake, so it is subject to the same
//! admission check as a promotion the master asks for.

use anyhow::{Context, Result};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long after a request a sandbox is safe from demotion.
///
/// Long enough to cover the master hearing about the request: a listening
/// node reports every 5 s, and a polling node reports as soon as a deferred
/// command shortens its hold. The master's idle thresholds are 30 s and up,
/// so this never decides on its own when a sandbox goes cold.
pub const GRACE: Duration = Duration::from_secs(15);

/// Predicting the next request, to have a snapshot image in the page cache
/// before it arrives.
///
/// A cold restore is a disk read: on bm 1.4 s for a compressed image that
/// restores in 0.35-0.7 s from cache. Plenty of sandboxes are called on a
/// schedule -- an agent polled every few minutes, a cron-driven job -- and
/// for those the next arrival is easy to guess. Reading the image shortly
/// before costs a read the restore would have done anyway; guessing wrong
/// costs one read and nothing else.
pub mod predict {
    use std::time::Duration;
    /// Arrivals kept per sandbox.
    pub const HISTORY: usize = 8;
    /// Intervals looked at; all of them must agree.
    pub const INTERVALS: usize = 3;
    /// How far apart the longest and shortest interval may be.
    pub const JITTER: f64 = 1.25;
    /// Shorter periods than this never let a sandbox reach Snapshot.
    pub const MIN_PERIOD: Duration = Duration::from_secs(30);
    /// How long before the predicted arrival to start reading.
    pub const LEAD: Duration = Duration::from_secs(20);
}

type WakeFn = dyn Fn(&str) -> Result<()> + Send + Sync;

pub struct Wakeups {
    /// One per sandbox, held across any tier change -- a wake, or a command
    /// from the master -- so the two never run runsc on one sandbox at once.
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// When each sandbox last had a request through the gateway.
    seen: Mutex<HashMap<String, Instant>>,
    /// Requests the master has not been told about yet.
    unreported: Mutex<HashSet<String>>,
    /// Restores read whole images from disk, so a burst of wakes for
    /// different sandboxes is queued rather than run all at once and left
    /// to fight over the same disk.
    restores: tokio::sync::Semaphore,
    wake: Box<WakeFn>,
    /// Signalled after every wake, so a polling node can tell the master at
    /// once rather than on its next poll.
    pub woke: tokio::sync::Notify,
    /// When each burst of traffic began: the first request after at least
    /// `GRACE` of quiet. What `due_prefetch` predicts from.
    arrivals: Mutex<HashMap<String, VecDeque<Instant>>>,
    /// The arrival a prefetch was last done after, so one is done per cycle.
    prefetched: Mutex<HashMap<String, Instant>>,
}

impl Wakeups {
    /// `wake` brings a sandbox to Hot and returns when it is serving. It
    /// blocks, and is called with that sandbox's lock held.
    pub fn new(wake: impl Fn(&str) -> Result<()> + Send + Sync + 'static) -> Self {
        let parallel = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2)
            .max(2);
        Self {
            locks: Mutex::default(),
            seen: Mutex::default(),
            unreported: Mutex::default(),
            restores: tokio::sync::Semaphore::new(parallel),
            wake: Box::new(wake),
            woke: tokio::sync::Notify::new(),
            arrivals: Mutex::default(),
            prefetched: Mutex::default(),
        }
    }

    /// The lock for one sandbox's tier.
    pub fn lock(&self, id: &str) -> Arc<Mutex<()>> {
        self.locks.lock().entry(id.to_string()).or_default().clone()
    }

    /// A request for this sandbox reached the gateway.
    pub fn saw(&self, id: &str) {
        let now = Instant::now();
        let prev = self.seen.lock().insert(id.to_string(), now);
        self.unreported.lock().insert(id.to_string());
        // `seen` forgets anything older than GRACE, so no entry also means
        // a quiet spell.
        if prev.is_none_or(|p| now.saturating_duration_since(p) >= GRACE) {
            self.arrived(id, now);
        }
    }

    fn arrived(&self, id: &str, at: Instant) {
        let mut all = self.arrivals.lock();
        let h = all.entry(id.to_string()).or_default();
        h.push_back(at);
        while h.len() > predict::HISTORY {
            h.pop_front();
        }
    }

    /// Sandboxes whose next burst of traffic is due within `predict::LEAD`,
    /// each returned once per cycle.
    pub fn due_prefetch(&self, now: Instant) -> Vec<String> {
        use predict::*;
        let all = self.arrivals.lock();
        let mut done = self.prefetched.lock();
        let mut due = Vec::new();
        for (id, h) in all.iter() {
            if h.len() < INTERVALS + 1 {
                continue;
            }
            let recent: Vec<Instant> = h.iter().rev().take(INTERVALS + 1).copied().collect();
            let gaps: Vec<Duration> = recent.windows(2).map(|w| w[0] - w[1]).collect();
            let (lo, hi) = (gaps.iter().min().unwrap(), gaps.iter().max().unwrap());
            if *lo < MIN_PERIOD || hi.as_secs_f64() > lo.as_secs_f64() * JITTER {
                continue;
            }
            let period = gaps.iter().sum::<Duration>() / gaps.len() as u32;
            let last = recent[0];
            let next = last + period;
            // Inside the lead window, and not so late that the arrival was
            // plainly missed.
            if now + LEAD < next || now > next + period / 2 {
                continue;
            }
            if done.get(id) == Some(&last) {
                continue;
            }
            done.insert(id.clone(), last);
            due.push(id.clone());
        }
        due.sort();
        due
    }

    /// How much longer this sandbox is protected from demotion, if at all.
    pub fn in_grace(&self, id: &str, now: Instant) -> Option<Duration> {
        let at = *self.seen.lock().get(id)?;
        GRACE
            .checked_sub(now.saturating_duration_since(at))
            .filter(|d| !d.is_zero())
    }

    /// The sandboxes to report as used, emptied as it is read.
    pub fn take_traffic(&self) -> Vec<String> {
        let now = Instant::now();
        // Pruned here because this runs on every report: nothing outside
        // the grace window is consulted again.
        self.seen
            .lock()
            .retain(|_, at| now.saturating_duration_since(*at) < GRACE);
        let mut ids: Vec<String> = self.unreported.lock().drain().collect();
        ids.sort();
        ids
    }

    /// A destroyed sandbox needs none of this any more.
    pub fn forget(&self, id: &str) {
        self.locks.lock().remove(id);
        self.seen.lock().remove(id);
        self.unreported.lock().remove(id);
        self.arrivals.lock().remove(id);
        self.prefetched.lock().remove(id);
    }

    /// Wake a sandbox and wait until it is serving. A sandbox that is
    /// already hot costs one observe: `set_tier` returns at once when the
    /// tier already matches, which is also what makes a burst of requests
    /// for one cold sandbox wake it once rather than once each.
    pub async fn wake(self: &Arc<Self>, id: &str) -> Result<Duration> {
        let _permit = self.restores.acquire().await.context("shutting down")?;
        let me = self.clone();
        let id = id.to_string();
        let me2 = self.clone();
        let took = tokio::task::spawn_blocking(move || {
            let lock = me.lock(&id);
            let _held = lock.lock();
            let t0 = Instant::now();
            (me.wake)(&id)?;
            Ok::<_, anyhow::Error>(t0.elapsed())
        })
        .await
        .context("the wake task panicked")??;
        me2.woke.notify_one();
        Ok(took)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn traffic_is_reported_once() {
        let w = Wakeups::new(|_| Ok(()));
        w.saw("a");
        w.saw("b");
        w.saw("a");
        assert_eq!(w.take_traffic(), vec!["a".to_string(), "b".to_string()]);
        assert!(w.take_traffic().is_empty(), "already reported");
    }

    #[test]
    fn grace_runs_out() {
        let w = Wakeups::new(|_| Ok(()));
        assert_eq!(w.in_grace("a", Instant::now()), None, "never seen");
        w.saw("a");
        let left = w.in_grace("a", Instant::now()).expect("just seen");
        assert!(left <= GRACE && left > GRACE - Duration::from_secs(1));
        assert_eq!(w.in_grace("a", Instant::now() + GRACE), None);
    }

    /// A sandbox called every 60 s is prefetched once, about 20 s before
    /// its next call; an irregular one never is.
    #[test]
    fn a_periodic_sandbox_is_prefetched_ahead_of_its_next_call() {
        let w = Wakeups::new(|_| Ok(()));
        let t0 = Instant::now();
        let s = Duration::from_secs;
        for k in 0..4 {
            w.arrived("cron", t0 + s(60 * k));
        }
        for k in [0, 40, 200, 230] {
            w.arrived("chaos", t0 + s(k));
        }
        let last = t0 + s(180);
        assert!(w.due_prefetch(last + s(30)).is_empty(), "too early");
        assert_eq!(w.due_prefetch(last + s(45)), vec!["cron".to_string()]);
        assert!(w.due_prefetch(last + s(50)).is_empty(), "once per cycle");
        assert!(
            w.due_prefetch(last + s(100)).is_empty(),
            "missed: no late read"
        );
    }

    #[test]
    fn too_short_a_period_is_not_worth_predicting() {
        let w = Wakeups::new(|_| Ok(()));
        let t0 = Instant::now();
        for k in 0..4 {
            w.arrived("busy", t0 + Duration::from_secs(20 * k));
        }
        assert!(w.due_prefetch(t0 + Duration::from_secs(75)).is_empty());
    }

    #[test]
    fn forgetting_drops_everything() {
        let w = Wakeups::new(|_| Ok(()));
        w.saw("a");
        w.forget("a");
        assert_eq!(w.in_grace("a", Instant::now()), None);
        assert!(w.take_traffic().is_empty());
    }

    /// The point of the per-sandbox lock: a wake must not run runsc while
    /// a command from the master is changing the same sandbox's tier.
    #[tokio::test]
    async fn a_wake_waits_for_a_tier_change_in_progress() {
        let running = Arc::new(AtomicUsize::new(0));
        let overlapped = Arc::new(AtomicUsize::new(0));
        let (r, o) = (running.clone(), overlapped.clone());
        let w = Arc::new(Wakeups::new(move |_| {
            if r.fetch_add(1, Ordering::SeqCst) > 0 {
                o.fetch_add(1, Ordering::SeqCst);
            }
            std::thread::sleep(Duration::from_millis(20));
            r.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }));

        // Held by "the master's command" on another thread for 100 ms.
        let lock = w.lock("sb");
        let (taken, is_taken) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _held = lock.lock();
            taken.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(100));
        });
        is_taken.recv().unwrap();
        let t0 = Instant::now();
        w.wake("sb").await.unwrap();
        assert!(
            t0.elapsed() >= Duration::from_millis(80),
            "the wake ran through a held lock"
        );
        holder.join().unwrap();

        // And many wakes for one sandbox run one at a time.
        let all: Vec<_> = (0..8)
            .map(|_| {
                let w = w.clone();
                tokio::spawn(async move { w.wake("sb").await })
            })
            .collect();
        for h in all {
            h.await.unwrap().unwrap();
        }
        assert_eq!(overlapped.load(Ordering::SeqCst), 0);
    }
}
