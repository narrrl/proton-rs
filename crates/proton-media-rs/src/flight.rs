//! One in-flight call per key, however many callers ask for it.
//!
//! Two places need this and both are hot:
//!
//! * **Opening a stream.** A cold open costs a link-details fetch, an S2K
//!   ancestor-key unlock and a revision listing. The UI's "play" click and the
//!   player's first read arrive within milliseconds of each other; without
//!   deduplication they pay for that twice and race to insert into the LRU.
//! * **Fetching a block.** Read-ahead runs *ahead* of the demand read by
//!   definition, so the two collide constantly on the block the player is about
//!   to want. Fetching it twice doubles the bytes off the wire for no gain.
//!
//! The entry is removed by a guard rather than by the leader's happy path, so a
//! cancelled call (a dropped future — the player abandoning a seek, say) cannot
//! leave a key permanently marked as in flight.

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::watch;

/// The result as a waiter sees it: the value, or the leader's error message.
type Outcome<V> = Option<std::result::Result<V, String>>;

/// The failure a waiter sees: the leader's error, as its message.
///
/// Errors are rarely `Clone`, so only the leader gets its typed error. A
/// caller's error type takes this through `From`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Shared(pub String);

/// Deduplicates concurrent calls that share a key.
pub struct SingleFlight<K, V> {
    calls: Mutex<HashMap<K, watch::Receiver<Outcome<V>>>>,
}

impl<K, V> SingleFlight<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    pub fn new() -> Self {
        Self {
            calls: Mutex::new(HashMap::new()),
        }
    }

    /// Run `work` for `key`, or wait for the call already running it.
    ///
    /// Whoever arrives first is the leader and actually runs the future; the
    /// rest wait on its result. The leader gets its own typed error, waiters get
    /// a [`Shared`] carrying its message.
    pub async fn run<E, F, Fut>(self: &Arc<Self>, key: K, work: F) -> Result<V, E>
    where
        E: std::fmt::Display + From<Shared>,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, E>>,
    {
        let role = {
            let mut calls = self.lock();
            match calls.get(&key) {
                Some(receiver) => Role::Waiter(receiver.clone()),
                None => {
                    let (sender, receiver) = watch::channel(None);
                    calls.insert(key.clone(), receiver);
                    Role::Leader(sender)
                }
            }
        };

        match role {
            Role::Leader(sender) => {
                // Removes the map entry on *every* exit path, including the one
                // where this future is dropped mid-await.
                let _guard = LeaderGuard {
                    flight: Arc::clone(self),
                    key: Some(key),
                };

                let result = work().await;
                let shared = match &result {
                    Ok(value) => Ok(value.clone()),
                    Err(e) => Err(e.to_string()),
                };
                // Waiters that already hold a receiver are woken here; ones that
                // arrive after the guard drops start a fresh call.
                let _ = sender.send(Some(shared));
                result
            }

            Role::Waiter(mut receiver) => loop {
                // Cloned out before the await: holding the borrow across it
                // would deadlock the sender.
                let seen = receiver.borrow_and_update().clone();
                if let Some(result) = seen {
                    return result.map_err(|message| Shared(message).into());
                }
                if receiver.changed().await.is_err() {
                    return Err(Shared(
                        "the task loading this was cancelled before it finished".into(),
                    )
                    .into());
                }
            },
        }
    }

    /// Run `work` for `key`, or take the result of the call already running it.
    ///
    /// For values that carry their own outcome — typically a `Result` whose
    /// error is `Clone` — so every caller gets the leader's typed result rather
    /// than a [`Shared`] message. A waiter whose leader is cancelled before it
    /// publishes runs `work` itself instead of failing; it does not take over
    /// the key, so a burst of cancellations cannot chain into each other.
    pub async fn join<F, Fut>(self: &Arc<Self>, key: K, work: F) -> V
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = V>,
    {
        let role = {
            let mut calls = self.lock();
            match calls.get(&key) {
                Some(receiver) => Role::Waiter(receiver.clone()),
                None => {
                    let (sender, receiver) = watch::channel(None);
                    calls.insert(key.clone(), receiver);
                    Role::Leader(sender)
                }
            }
        };

        match role {
            Role::Leader(sender) => {
                let guard = LeaderGuard {
                    flight: Arc::clone(self),
                    key: Some(key),
                };
                let value = work().await;
                // Retire the key before publishing, so a caller woken by the
                // send that comes straight back starts a fresh call instead of
                // joining a finished one.
                drop(guard);
                let _ = sender.send(Some(Ok(value.clone())));
                value
            }

            Role::Waiter(mut receiver) => {
                loop {
                    let seen = receiver.borrow_and_update().clone();
                    if let Some(Ok(value)) = seen {
                        return value;
                    }
                    if receiver.changed().await.is_err() {
                        break;
                    }
                }
                work().await
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<K, watch::Receiver<Outcome<V>>>> {
        // A panicking caller must not wedge every future call on this key. The
        // map is a plain index with no invariant a panic could break.
        self.calls.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<K, V> Default for SingleFlight<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    fn default() -> Self {
        Self::new()
    }
}

enum Role<V> {
    Leader(watch::Sender<Outcome<V>>),
    Waiter(watch::Receiver<Outcome<V>>),
}

struct LeaderGuard<K: Eq + Hash + Clone, V: Clone> {
    flight: Arc<SingleFlight<K, V>>,
    key: Option<K>,
}

impl<K: Eq + Hash + Clone, V: Clone> Drop for LeaderGuard<K, V> {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            self.flight.lock().remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[derive(Debug, thiserror::Error)]
    enum Error {
        #[error("{0}")]
        NotFound(String),
        #[error(transparent)]
        Shared(#[from] Shared),
    }

    /// The point of the whole file: N callers, one execution.
    #[tokio::test]
    async fn concurrent_callers_of_one_key_run_the_work_once() {
        let flight: Arc<SingleFlight<u32, u32>> = Arc::new(SingleFlight::new());
        let runs = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Notify::new());

        let mut handles = Vec::new();
        for _ in 0..8 {
            let flight = Arc::clone(&flight);
            let runs = Arc::clone(&runs);
            let gate = Arc::clone(&gate);
            handles.push(tokio::spawn(async move {
                flight
                    .run(7, || async {
                        runs.fetch_add(1, Ordering::SeqCst);
                        // Held open so every caller is definitely waiting.
                        gate.notified().await;
                        Ok::<_, Error>(42)
                    })
                    .await
            }));
        }

        // Let all eight reach the map before the leader completes.
        tokio::task::yield_now().await;
        gate.notify_waiters();

        for handle in handles {
            assert_eq!(handle.await.expect("join").expect("value"), 42);
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1, "work ran more than once");
    }

    /// Different keys must not serialize against each other.
    #[tokio::test]
    async fn different_keys_run_independently() {
        let flight: Arc<SingleFlight<u32, u32>> = Arc::new(SingleFlight::new());
        let runs = Arc::new(AtomicUsize::new(0));

        for key in 0..4 {
            let runs = Arc::clone(&runs);
            let value = flight
                .run(key, || async {
                    runs.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, Error>(key * 2)
                })
                .await
                .expect("value");
            assert_eq!(value, key * 2);
        }
        assert_eq!(runs.load(Ordering::SeqCst), 4);
    }

    /// A waiter gets the leader's message, not a success and not a hang.
    #[tokio::test]
    async fn a_waiter_sees_the_leaders_failure() {
        let flight: Arc<SingleFlight<u32, u32>> = Arc::new(SingleFlight::new());
        let gate = Arc::new(tokio::sync::Notify::new());

        let leader = {
            let flight = Arc::clone(&flight);
            let gate = Arc::clone(&gate);
            tokio::spawn(async move {
                flight
                    .run(1, || async {
                        gate.notified().await;
                        Err(Error::NotFound("no such block".into()))
                    })
                    .await
            })
        };

        tokio::task::yield_now().await;
        let waiter = {
            let flight = Arc::clone(&flight);
            tokio::spawn(async move { flight.run(1, || async { Ok::<_, Error>(0) }).await })
        };
        tokio::task::yield_now().await;
        gate.notify_waiters();

        assert!(leader.await.expect("join").is_err());
        let error = waiter
            .await
            .expect("join")
            .expect_err("waiter must fail too");
        assert!(
            error.to_string().contains("no such block"),
            "waiter should carry the leader's reason: {error}"
        );
    }

    /// A failed call must not poison the key — the next caller retries.
    #[tokio::test]
    async fn a_failed_call_leaves_the_key_retryable() {
        let flight: Arc<SingleFlight<u32, u32>> = Arc::new(SingleFlight::new());

        let first = flight
            .run(1, || async { Err(Error::NotFound("transient".into())) })
            .await;
        assert!(first.is_err());

        let second = flight
            .run(1, || async { Ok::<_, Error>(9) })
            .await
            .expect("retry");
        assert_eq!(second, 9);
    }

    /// `join` hands every caller the leader's typed result, error included.
    #[tokio::test]
    async fn join_shares_the_typed_result() {
        let flight: Arc<SingleFlight<u32, Result<u32, u8>>> = Arc::new(SingleFlight::new());
        let runs = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Notify::new());

        let mut handles = Vec::new();
        for _ in 0..4 {
            let flight = Arc::clone(&flight);
            let runs = Arc::clone(&runs);
            let gate = Arc::clone(&gate);
            handles.push(tokio::spawn(async move {
                flight
                    .join(3, || async {
                        runs.fetch_add(1, Ordering::SeqCst);
                        gate.notified().await;
                        Err(7)
                    })
                    .await
            }));
        }
        tokio::task::yield_now().await;
        gate.notify_waiters();

        for handle in handles {
            assert_eq!(handle.await.expect("join"), Err(7));
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1, "work ran more than once");
        assert!(flight.lock().is_empty(), "entry outlived the call");
    }

    /// A `join` waiter whose leader is dropped does the work itself.
    #[tokio::test]
    async fn join_waiter_of_a_cancelled_leader_runs_the_work() {
        let flight: Arc<SingleFlight<u32, u32>> = Arc::new(SingleFlight::new());

        let leader = {
            let flight = Arc::clone(&flight);
            tokio::spawn(async move {
                flight
                    .join(1, || async {
                        std::future::pending::<()>().await;
                        0
                    })
                    .await
            })
        };
        tokio::task::yield_now().await;
        let waiter = {
            let flight = Arc::clone(&flight);
            tokio::spawn(async move { flight.join(1, || async { 11 }).await })
        };
        tokio::task::yield_now().await;
        leader.abort();

        assert_eq!(waiter.await.expect("join"), 11);
        assert!(flight.lock().is_empty());
    }

    /// A dropped leader must not leave the key marked in-flight forever — the
    /// player cancels reads whenever the viewer seeks away.
    #[tokio::test]
    async fn a_cancelled_leader_does_not_wedge_the_key() {
        let flight: Arc<SingleFlight<u32, u32>> = Arc::new(SingleFlight::new());

        {
            let pending = flight.run(1, || async {
                std::future::pending::<()>().await;
                Ok::<_, Error>(0)
            });
            // Poll once so it becomes the leader, then drop it unfinished.
            futures::pin_mut!(pending);
            let mut context = std::task::Context::from_waker(futures::task::noop_waker_ref());
            assert!(std::future::Future::poll(pending.as_mut(), &mut context).is_pending());
        }

        assert!(
            flight.lock().is_empty(),
            "entry outlived the cancelled call"
        );
        assert_eq!(
            flight
                .run(1, || async { Ok::<_, Error>(5) })
                .await
                .expect("retry"),
            5
        );
    }
}
