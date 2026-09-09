use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;

use crate::git::{PreparedSync, Settings, SyncPlan, apply_sync, inspect_repository, prepare_sync};
use crate::model::{Inspection, Repository, SyncResult};

#[derive(Clone, Debug)]
pub enum EngineEvent {
    InspectStarted(usize),
    InspectFinished(usize, Inspection),
    SyncFetching(usize),
    SyncPrepared(usize, SyncPlan),
    SyncChecksFinished(usize),
    SyncUpdating(usize, SyncPlan),
    SyncFinished(usize, SyncResult),
}

pub type EventSink = Arc<dyn Fn(EngineEvent) + Send + Sync>;

#[derive(Clone, Debug)]
pub struct SyncLimiter {
    inner: Arc<(Mutex<usize>, Condvar)>,
    limit: usize,
}

impl SyncLimiter {
    pub fn new(limit: usize) -> Self {
        Self {
            inner: Arc::new((Mutex::new(0), Condvar::new())),
            limit: limit.max(1),
        }
    }

    fn acquire(&self) -> SyncPermit {
        let (active, available) = &*self.inner;
        let mut active = active.lock().unwrap_or_else(|error| error.into_inner());
        while *active >= self.limit {
            active = available
                .wait(active)
                .unwrap_or_else(|error| error.into_inner());
        }
        *active += 1;
        SyncPermit {
            limiter: self.clone(),
        }
    }
}

struct SyncPermit {
    limiter: SyncLimiter,
}

impl Drop for SyncPermit {
    fn drop(&mut self) {
        let (active, available) = &*self.limiter.inner;
        let mut active = active.lock().unwrap_or_else(|error| error.into_inner());
        *active = active.saturating_sub(1);
        available.notify_one();
    }
}

pub fn inspect_all(
    repositories: &[Repository],
    refresh: bool,
    settings: Settings,
    sink: EventSink,
) -> Vec<Inspection> {
    let count = repositories.len();
    if count == 0 {
        return Vec::new();
    }
    let repositories = Arc::new(repositories.to_vec());
    let next = Arc::new(AtomicUsize::new(0));
    let (sender, receiver) = mpsc::channel();
    let worker_count = settings.jobs.min(count);
    let mut workers = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let repositories = Arc::clone(&repositories);
        let next = Arc::clone(&next);
        let sender = sender.clone();
        let sink = Arc::clone(&sink);
        workers.push(thread::spawn(move || {
            loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= repositories.len() {
                    break;
                }
                sink(EngineEvent::InspectStarted(index));
                let result = inspect_repository(
                    &repositories[index],
                    refresh,
                    settings.fetch_attempts,
                    settings.terminal_prompt,
                    settings.command_timeout,
                );
                sink(EngineEvent::InspectFinished(index, result.clone()));
                if sender.send((index, result)).is_err() {
                    break;
                }
            }
        }));
    }
    drop(sender);
    let mut results = vec![Inspection::default(); count];
    for (index, result) in receiver {
        results[index] = result;
    }
    for worker in workers {
        worker.join().expect("inspection worker panicked");
    }
    results
}

pub fn sync_all(
    repositories: &[Repository],
    settings: Settings,
    sink: EventSink,
) -> Vec<SyncResult> {
    sync_all_with_limiter(
        repositories,
        settings,
        sink,
        SyncLimiter::new(settings.jobs),
    )
}

pub fn sync_all_with_limiter(
    repositories: &[Repository],
    settings: Settings,
    sink: EventSink,
    limiter: SyncLimiter,
) -> Vec<SyncResult> {
    let count = repositories.len();
    if count == 0 {
        return Vec::new();
    }
    let repositories = Arc::new(repositories.to_vec());
    let next = Arc::new(AtomicUsize::new(0));
    let (sender, receiver) = mpsc::channel();
    let worker_count = settings.jobs.min(count);
    let mut workers = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let repositories = Arc::clone(&repositories);
        let next = Arc::clone(&next);
        let sender = sender.clone();
        let sink = Arc::clone(&sink);
        let limiter = limiter.clone();
        workers.push(thread::spawn(move || {
            loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= repositories.len() {
                    break;
                }
                let _permit = limiter.acquire();
                sink(EngineEvent::SyncFetching(index));
                let prepared = prepare_sync(
                    &repositories[index],
                    settings.fetch_attempts,
                    settings.terminal_prompt,
                    settings.command_timeout,
                );
                if sender.send((index, prepared)).is_err() {
                    break;
                }
            }
        }));
    }
    drop(sender);
    let mut prepared = vec![None; count];
    for (index, result) in receiver {
        match &result {
            PreparedSync::Update(plan) => {
                sink(EngineEvent::SyncPrepared(index, plan.clone()));
            }
            PreparedSync::Terminal(result) => {
                sink(EngineEvent::SyncFinished(index, result.clone()));
            }
        }
        prepared[index] = Some(result);
    }
    for worker in workers {
        let _ = worker.join();
    }

    sink(EngineEvent::SyncChecksFinished(
        prepared
            .iter()
            .filter(|result| matches!(result, Some(PreparedSync::Update(_))))
            .count(),
    ));

    let mut results = Vec::with_capacity(count);
    for (index, prepared) in prepared.into_iter().enumerate() {
        match prepared.expect("sync worker did not return a result") {
            PreparedSync::Terminal(result) => {
                results.push(result);
            }
            PreparedSync::Update(plan) => {
                sink(EngineEvent::SyncUpdating(index, plan.clone()));
                let result = apply_sync(&plan);
                sink(EngineEvent::SyncFinished(index, result.clone()));
                results.push(result);
            }
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::time::Duration;

    #[test]
    fn shared_limiter_caps_parallel_prepare_work() {
        let limiter = SyncLimiter::new(2);
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(5));
        let mut workers = Vec::new();
        for _ in 0..4 {
            let limiter = limiter.clone();
            let active = Arc::clone(&active);
            let peak = Arc::clone(&peak);
            let start = Arc::clone(&start);
            workers.push(thread::spawn(move || {
                start.wait();
                let _permit = limiter.acquire();
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(current, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(30));
                active.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        start.wait();
        for worker in workers {
            worker.join().unwrap();
        }

        assert_eq!(peak.load(Ordering::SeqCst), 2);
    }
}
