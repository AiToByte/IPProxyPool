//! Local tenant shutdown stages. Pingora owns listener/runtime termination.
//!
//! A completed local report only covers admission, the periodic tenant writer,
//! and one final flush. It does not synchronize this watcher with process exit.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

use crate::health::RuntimeHealthState;
use crate::tenant::TenantManager;

#[derive(Debug)]
pub(super) enum BoundedFlushError<E> {
    Failed(E),
    TimedOut,
}

pub(super) async fn bounded_flush<F, T, E>(
    flush: F,
    timeout: Duration,
) -> Result<T, BoundedFlushError<E>>
where
    F: std::future::Future<Output = Result<T, E>>,
{
    match tokio::time::timeout(timeout, flush).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(BoundedFlushError::Failed(error)),
        Err(_) => Err(BoundedFlushError::TimedOut),
    }
}

/// Own the periodic writer throughout shutdown. Drop requests cancellation when
/// the signal watcher is itself cancelled, so its task cannot be detached.
pub(super) struct TenantFlusher {
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
    abort_on_drop: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum FlusherStop {
    NotRunning,
    Joined,
    Cancelled,
    Failed,
    TimedOut,
}

impl TenantFlusher {
    pub(super) fn new((stop, task): (oneshot::Sender<()>, JoinHandle<()>)) -> Self {
        Self {
            stop: Some(stop),
            task: Some(task),
            abort_on_drop: true,
        }
    }

    async fn stop(mut self, budget: Duration) -> FlusherStop {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let Some(task) = self.task.as_mut() else {
            return FlusherStop::NotRunning;
        };
        let outcome = match tokio::time::timeout(budget, &mut *task).await {
            Ok(Ok(())) => FlusherStop::Joined,
            Ok(Err(error)) if error.is_cancelled() => FlusherStop::Cancelled,
            Ok(Err(_)) => FlusherStop::Failed,
            Err(_) => {
                task.abort();
                match tokio::time::timeout(budget, &mut *task).await {
                    Ok(Err(error)) if error.is_cancelled() => FlusherStop::Cancelled,
                    Ok(Ok(())) => FlusherStop::Joined,
                    Ok(Err(_)) => FlusherStop::Failed,
                    Err(_) => FlusherStop::TimedOut,
                }
            }
        };
        // If still pending, Drop makes a final cancellation request. Caller skips
        // final flush unless it observed the JoinHandle terminate above.
        self.abort_on_drop = matches!(outcome, FlusherStop::TimedOut);
        outcome
    }
}

impl Drop for TenantFlusher {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if self.abort_on_drop {
            if let Some(task) = self.task.as_ref() {
                task.abort();
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum FinalFlush {
    Completed(usize),
    Failed,
    TimedOut,
    Skipped,
}

#[derive(Debug)]
pub(super) struct ShutdownReport {
    quiesced: bool,
    flusher: FlusherStop,
    flush: FinalFlush,
    dirty_remaining: bool,
}

impl ShutdownReport {
    pub(super) fn locally_complete(&self) -> bool {
        self.quiesced
            && matches!(self.flusher, FlusherStop::Joined | FlusherStop::NotRunning)
            && matches!(self.flush, FinalFlush::Completed(_))
            && !self.dirty_remaining
    }
}

/// The existing watcher sequence, extracted so tests exercise production wiring.
pub(super) async fn shutdown_tenants(
    health: Arc<RuntimeHealthState>,
    stop: watch::Sender<bool>,
    tenants: Arc<TenantManager>,
    flusher: Option<TenantFlusher>,
    budget: Duration,
) -> ShutdownReport {
    health.request_drain();
    let _ = stop.send(true);
    let quiesced = health.wait_for_quiescence(budget).await;
    let flusher = match flusher {
        Some(flusher) => flusher.stop(budget).await,
        None => FlusherStop::NotRunning,
    };
    let flush = if !quiesced || matches!(flusher, FlusherStop::TimedOut) {
        FinalFlush::Skipped
    } else {
        match bounded_flush(tenants.flush_dirty(), budget).await {
            Ok(count) => FinalFlush::Completed(count),
            Err(BoundedFlushError::Failed(_)) => FinalFlush::Failed,
            Err(BoundedFlushError::TimedOut) => FinalFlush::TimedOut,
        }
    };
    ShutdownReport {
        quiesced,
        flusher,
        flush,
        dirty_remaining: tenants.has_dirty_state(),
    }
}

#[cfg(test)]
mod tests;
