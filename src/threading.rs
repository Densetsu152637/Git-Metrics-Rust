//! Concurrency primitives: request batching, bounded worker pools, and the
//! progress channel.
//!
//! Submitted work runs on Tokio tasks that are aborted when their handles are
//! dropped, so cancelling an analysis also cancels its queued and running
//! commands (whose child processes are killed on drop).

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};

use futures_util::FutureExt;
use tokio::sync::{mpsc, watch, Semaphore};
use tokio::task::JoinHandle;

/// Capacity of the per-request progress queue.
pub const PROGRESS_CAPACITY: usize = 1000;

/// Queues a progress update. Updates never stall repository work: when the
/// queue is full or the receiver is gone the update is dropped.
pub fn emit(progress: &mpsc::Sender<String>, message: impl Into<String>) {
    let _ = progress.try_send(message.into());
}

/// Prints one line to stdout.
pub fn safe_print(message: &str) {
    println!("{message}");
}

// ---------------------------------------------------------------------------
// Request batching
// ---------------------------------------------------------------------------

/// The shared request was cancelled or panicked before it produced a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchAborted;

#[derive(Clone)]
enum Outcome<V> {
    Completed(V),
    Aborted,
}

type Slot<V> = watch::Receiver<Option<Outcome<V>>>;

struct Entry<V> {
    slot: Slot<V>,
    followers: usize,
}

/// Registry of currently running actions. Callers using the same key share the
/// result of one action; completed actions are not cached.
pub struct AwaitBatch<K, V> {
    active: Mutex<HashMap<K, Entry<V>>>,
}

impl<K, V> Default for AwaitBatch<K, V> {
    fn default() -> Self {
        Self {
            active: Mutex::new(HashMap::new()),
        }
    }
}

struct Leader<'a, K: Hash + Eq, V: Clone> {
    batch: &'a AwaitBatch<K, V>,
    key: K,
    sender: watch::Sender<Option<Outcome<V>>>,
    finished: bool,
}

impl<K: Hash + Eq, V: Clone> Leader<'_, K, V> {
    /// Removes the key first so later callers start a fresh action, then
    /// broadcasts to everyone already waiting.
    fn finish(&mut self, outcome: Outcome<V>) {
        self.finished = true;
        self.batch
            .active
            .lock()
            .expect("batch lock poisoned")
            .remove(&self.key);
        self.sender.send_replace(Some(outcome));
    }
}

impl<K: Hash + Eq, V: Clone> Drop for Leader<'_, K, V> {
    fn drop(&mut self) {
        if !self.finished {
            self.finish(Outcome::Aborted);
        }
    }
}

impl<K: Hash + Eq + Clone, V: Clone> AwaitBatch<K, V> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs `action` at most once at a time per key. The first caller is the
    /// leader; concurrent followers await the same broadcast result. The key is
    /// always removed afterwards, so a later call retries.
    pub async fn run<F, Fut>(&self, key: K, action: F) -> Result<V, BatchAborted>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = V>,
    {
        let follower_slot = {
            let mut active = self.active.lock().expect("batch lock poisoned");
            match active.get_mut(&key) {
                Some(existing) => {
                    existing.followers += 1;
                    Err(existing.slot.clone())
                }
                None => {
                    let (sender, receiver) = watch::channel(None);
                    active.insert(
                        key.clone(),
                        Entry {
                            slot: receiver,
                            followers: 0,
                        },
                    );
                    Ok(sender)
                }
            }
        };

        match follower_slot {
            Ok(sender) => {
                let mut leader = Leader {
                    batch: self,
                    key,
                    sender,
                    finished: false,
                };
                let value = action().await;
                leader.finish(Outcome::Completed(value.clone()));
                Ok(value)
            }
            Err(mut receiver) => {
                let outcome = receiver.wait_for(Option::is_some).await;
                match outcome.as_deref() {
                    Ok(Some(Outcome::Completed(value))) => Ok(value.clone()),
                    _ => Err(BatchAborted),
                }
            }
        }
    }

    /// Followers that have joined the in-flight action for `key`.
    #[cfg(test)]
    fn followers(&self, key: &K) -> usize {
        self.active
            .lock()
            .expect("batch lock poisoned")
            .get(key)
            .map_or(0, |entry| entry.followers)
    }
}

// ---------------------------------------------------------------------------
// Worker pools
// ---------------------------------------------------------------------------

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

type TaskOutcome<T> = Result<Arc<T>, String>;

/// A submitted task. Awaiting is repeatable and every awaiter sees the same
/// value or failure. The task is aborted once the last handle is dropped.
pub struct Submitted<T> {
    receiver: watch::Receiver<Option<TaskOutcome<T>>>,
    _task: Arc<AbortOnDrop>,
}

impl<T> Clone for Submitted<T> {
    fn clone(&self) -> Self {
        Self {
            receiver: self.receiver.clone(),
            _task: Arc::clone(&self._task),
        }
    }
}

impl<T> Submitted<T> {
    pub async fn wait(&self) -> Result<Arc<T>, String> {
        let mut receiver = self.receiver.clone();
        let outcome = match receiver.wait_for(Option::is_some).await {
            Ok(outcome) => outcome.clone().expect("outcome checked by wait_for"),
            Err(_) => Err("task was cancelled before it produced a result".to_owned()),
        };
        outcome
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    let text = payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_owned());
    format!("worker task panicked: {text}")
}

/// A pool that runs at most `workers` tasks at once, in submission order.
#[derive(Clone)]
pub struct WorkerPool {
    name: Arc<str>,
    workers: usize,
    permits: Arc<Semaphore>,
}

impl WorkerPool {
    /// Creates a pool with `workers` slots, but never fewer than one so
    /// submitted work cannot deadlock.
    pub fn new(workers: usize, name: &str) -> Self {
        let workers = workers.max(1);
        Self {
            name: name.into(),
            workers,
            permits: Arc::new(Semaphore::new(workers)),
        }
    }

    pub fn num_workers(&self) -> usize {
        self.workers
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Queues an async task. Must be called inside a Tokio runtime.
    pub fn submit<T, Fut>(&self, work: Fut) -> Submitted<T>
    where
        T: Send + Sync + 'static,
        Fut: Future<Output = Result<T, String>> + Send + 'static,
    {
        let (sender, receiver) = watch::channel(None);
        let permits = Arc::clone(&self.permits);
        let task = tokio::spawn(async move {
            let Ok(_permit) = permits.acquire_owned().await else {
                return;
            };
            let outcome = match AssertUnwindSafe(work).catch_unwind().await {
                Ok(result) => result.map(Arc::new),
                Err(payload) => Err(panic_message(payload.as_ref())),
            };
            sender.send_replace(Some(outcome));
        });
        Submitted {
            receiver,
            _task: Arc::new(AbortOnDrop(task)),
        }
    }

    /// Queues CPU-bound work on Tokio's blocking threads, limited by this pool.
    pub fn submit_blocking<T, F>(&self, work: F) -> Submitted<T>
    where
        T: Send + Sync + 'static,
        F: FnOnce() -> Result<T, String> + Send + 'static,
    {
        self.submit(async move {
            match tokio::task::spawn_blocking(work).await {
                Ok(result) => result,
                Err(error) => Err(format!("worker task failed: {error}")),
            }
        })
    }
}

/// Runs `first` for every input in `first_pool` as soon as possible, then
/// feeds each result, in input order, to `second` in `second_pool`. Dropping
/// the returned future (or failing early) cancels everything still queued.
pub async fn pass_all_async<A, B, C, F1, Fut1, F2>(
    first_pool: &WorkerPool,
    second_pool: &WorkerPool,
    first: F1,
    second: F2,
    inputs: Vec<A>,
) -> Result<Vec<C>, String>
where
    B: Send + Sync + 'static,
    C: Clone + Send + Sync + 'static,
    F1: Fn(A) -> Fut1,
    Fut1: Future<Output = Result<B, String>> + Send + 'static,
    F2: Fn(Arc<B>) -> Result<C, String> + Send + Sync + 'static,
{
    let second = Arc::new(second);
    let firsts: Vec<Submitted<B>> = inputs
        .into_iter()
        .map(|input| first_pool.submit(first(input)))
        .collect();

    let mut results = Vec::with_capacity(firsts.len());
    for stage_one in firsts {
        let intermediate = stage_one.wait().await?;
        let second = Arc::clone(&second);
        let stage_two = second_pool.submit_blocking(move || second(intermediate));
        let value = stage_two.wait().await?;
        drop(stage_two);
        results.push(Arc::try_unwrap(value).unwrap_or_else(|shared| (*shared).clone()));
    }
    Ok(results)
}

/// [`pass_all_async`] for a single input.
pub async fn pass_through_async<A, B, C, F1, Fut1, F2>(
    first_pool: &WorkerPool,
    second_pool: &WorkerPool,
    first: F1,
    second: F2,
    input: A,
) -> Result<C, String>
where
    B: Send + Sync + 'static,
    C: Clone + Send + Sync + 'static,
    F1: Fn(A) -> Fut1,
    Fut1: Future<Output = Result<B, String>> + Send + 'static,
    F2: Fn(Arc<B>) -> Result<C, String> + Send + Sync + 'static,
{
    let mut values = pass_all_async(first_pool, second_pool, first, second, vec![input]).await?;
    Ok(values.remove(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::Notify;

    async fn within<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(30), future)
            .await
            .expect("test timed out")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_duplicates_share_one_action() {
        for _ in 0..20 {
            let batch = Arc::new(AwaitBatch::<String, usize>::new());
            let action_count = Arc::new(AtomicUsize::new(0));
            let started = Arc::new(Notify::new());
            let finish = Arc::new(Notify::new());
            let key = "same-repository".to_owned();

            let mut callers = Vec::new();
            for _ in 0..32 {
                let (batch, count, started, finish, key) = (
                    batch.clone(),
                    action_count.clone(),
                    started.clone(),
                    finish.clone(),
                    key.clone(),
                );
                callers.push(tokio::spawn(async move {
                    batch
                        .run(key, || async move {
                            count.fetch_add(1, Ordering::SeqCst);
                            started.notify_one();
                            finish.notified().await;
                            42
                        })
                        .await
                }));
            }
            within(started.notified()).await;
            // Every other caller must have joined the in-flight batch while the
            // leader is still blocked.
            within(async {
                while batch.followers(&key) < 31 {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            finish.notify_one();

            for caller in callers {
                assert_eq!(within(caller).await.unwrap(), Ok(42));
            }
            assert_eq!(action_count.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn completed_batches_are_not_cached_and_failures_retry() {
        let batch = AwaitBatch::<&str, Result<u8, String>>::new();
        assert_eq!(
            batch
                .run("repository", || async { Err("failed".to_owned()) })
                .await,
            Ok(Err("failed".to_owned()))
        );
        assert_eq!(batch.run("repository", || async { Ok(7) }).await, Ok(Ok(7)));
        assert_eq!(batch.run("repository", || async { Ok(8) }).await, Ok(Ok(8)));
    }

    #[tokio::test]
    async fn cancelled_leader_releases_followers_and_the_key() {
        let batch = Arc::new(AwaitBatch::<&'static str, u8>::new());
        let started = Arc::new(Notify::new());
        let leader = {
            let (batch, started) = (batch.clone(), started.clone());
            tokio::spawn(async move {
                batch
                    .run("key", || async move {
                        started.notify_one();
                        std::future::pending::<u8>().await
                    })
                    .await
            })
        };
        within(started.notified()).await;
        let follower = {
            let batch = batch.clone();
            tokio::spawn(async move { batch.run("key", || async { 1 }).await })
        };
        within(async {
            while batch.followers(&"key") < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        leader.abort();
        assert_eq!(within(follower).await.unwrap(), Err(BatchAborted));
        assert_eq!(batch.run("key", || async { 5 }).await, Ok(5));
    }

    #[tokio::test]
    async fn submitted_futures_are_reusable() {
        let pool = WorkerPool::new(1, "future-test");
        let future = pool.submit(async { Ok(11) });
        assert_eq!(*future.wait().await.unwrap(), 11);
        assert_eq!(*future.wait().await.unwrap(), 11);
    }

    #[tokio::test]
    async fn submitted_failures_reach_every_awaiter() {
        let pool = WorkerPool::new(1, "exception-test");
        let future = pool.submit::<u8, _>(async { Err("worker failed".to_owned()) });
        assert_eq!(future.wait().await.unwrap_err(), "worker failed");
        assert_eq!(future.wait().await.unwrap_err(), "worker failed");

        let panicking = pool.submit::<u8, _>(async { panic!("boom") });
        assert!(panicking.wait().await.unwrap_err().contains("boom"));
        // The pool survives a panicking task.
        assert_eq!(*pool.submit(async { Ok(1) }).wait().await.unwrap(), 1);
    }

    #[test]
    fn progress_queue_never_blocks_work() {
        let (sender, mut receiver) = mpsc::channel(1);
        emit(&sender, "first");
        emit(&sender, "dropped while full");
        assert_eq!(receiver.try_recv().unwrap(), "first");
        assert!(receiver.try_recv().is_err());
        drop(receiver);
        emit(&sender, "closed receiver is ignored");
    }

    #[test]
    fn pools_always_have_a_worker() {
        assert_eq!(WorkerPool::new(0, "minimum-worker-test").num_workers(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pool_limits_concurrency() {
        let pool = WorkerPool::new(2, "limit");
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let (running, peak) = (running.clone(), peak.clone());
                pool.submit(async move {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    running.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                })
            })
            .collect();
        for task in &tasks {
            task.wait().await.unwrap();
        }
        assert!(peak.load(Ordering::SeqCst) <= 2);
    }

    #[tokio::test]
    async fn two_stage_pipeline_preserves_order_and_fails_fast() {
        let (first, second) = (WorkerPool::new(3, "first"), WorkerPool::new(1, "second"));
        let values = pass_all_async(
            &first,
            &second,
            |value: u32| async move { Ok(value * 2) },
            |value: Arc<u32>| Ok(format!("v{value}")),
            vec![1, 2, 3],
        )
        .await
        .unwrap();
        assert_eq!(values, ["v2", "v4", "v6"]);

        let error = pass_all_async(
            &first,
            &second,
            |value: u32| async move {
                if value == 2 {
                    Err("stage one".to_owned())
                } else {
                    Ok(value)
                }
            },
            |value: Arc<u32>| Ok(*value),
            vec![1, 2, 3],
        )
        .await
        .unwrap_err();
        assert_eq!(error, "stage one");

        let single = pass_through_async(
            &first,
            &second,
            |value: u32| async move { Ok(value) },
            |value: Arc<u32>| -> Result<u32, String> { Err(format!("stage two {value}")) },
            5,
        )
        .await;
        assert_eq!(single, Err("stage two 5".to_owned()));
    }

    #[tokio::test]
    async fn dropping_a_pipeline_cancels_queued_work() {
        let pool = WorkerPool::new(1, "cancel");
        let started = Arc::new(AtomicUsize::new(0));
        let blocker = Arc::new(Notify::new());
        let pipeline = {
            let (pool, started, blocker) = (pool.clone(), started.clone(), blocker.clone());
            tokio::spawn(async move {
                pass_all_async(
                    &pool,
                    &pool,
                    |_: u8| {
                        let (started, blocker) = (started.clone(), blocker.clone());
                        async move {
                            started.fetch_add(1, Ordering::SeqCst);
                            blocker.notified().await;
                            Ok(())
                        }
                    },
                    |_: Arc<()>| Ok(()),
                    vec![0, 1, 2, 3],
                )
                .await
            })
        };
        within(async {
            while started.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        pipeline.abort();
        let _ = pipeline.await;
        // The single permit is released, so new work runs and queued work never started.
        assert_eq!(
            *within(pool.submit(async { Ok(9) }).wait()).await.unwrap(),
            9
        );
        assert_eq!(started.load(Ordering::SeqCst), 1);
    }
}
