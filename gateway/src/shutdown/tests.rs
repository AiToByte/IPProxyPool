use super::*;
use crate::tenant::{RecordingBackend, Store};
use std::future::{pending, poll_fn, Future};
use std::task::Poll;

const BUDGET: Duration = Duration::from_secs(2);
const KEY: &str = "shutdown-test-key";

fn persistent_tenant() -> (Arc<TenantManager>, Arc<RecordingBackend>) {
    let backend = RecordingBackend::new();
    let tenants = Arc::new(TenantManager::with_store(Store::Recording(backend.clone())));
    tenants.register_tenant("test", KEY, 10, 10, 10);
    (tenants, backend)
}

fn stop_channel() -> watch::Sender<bool> {
    watch::channel(false).0
}

/// Poll once to prove ordering without a scheduler-dependent sleep.
async fn assert_pending<F: Future>(mut future: std::pin::Pin<&mut F>) {
    assert!(poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx).is_pending())).await);
}

struct OnDrop(Option<oneshot::Sender<()>>);

impl Drop for OnDrop {
    fn drop(&mut self) {
        let _ = self.0.take().unwrap().send(());
    }
}

async fn held_flusher() -> (TenantFlusher, oneshot::Receiver<()>) {
    let (stop, _stop_rx) = oneshot::channel();
    let (entered, entered_rx) = oneshot::channel();
    let (dropped, dropped_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let _drop = OnDrop(Some(dropped));
        entered.send(()).unwrap();
        pending::<()>().await;
    });
    entered_rx.await.unwrap();
    (TenantFlusher::new((stop, task)), dropped_rx)
}

#[tokio::test]
async fn flusher_timeout_cancels_and_joins_before_returning() {
    let (flusher, mut dropped) = held_flusher().await;
    let cleanup = flusher.task.as_ref().unwrap().abort_handle();

    let result = flusher.stop(Duration::from_millis(10)).await;
    let destructor_observed = dropped.try_recv();
    cleanup.abort();

    assert_ne!(result, FlusherStop::Joined);
    assert_eq!(
        destructor_observed,
        Ok(()),
        "join must observe task destructor"
    );
}

#[tokio::test]
async fn dropping_shutdown_owner_aborts_flusher_while_drain_is_pending() {
    let (tenants, _backend) = persistent_tenant();
    let health = RuntimeHealthState::new();
    let _lease = health.try_admit().unwrap();
    let (flusher, dropped) = held_flusher().await;
    let cleanup = flusher.task.as_ref().unwrap().abort_handle();
    let mut shutdown = Box::pin(shutdown_tenants(
        health,
        stop_channel(),
        tenants,
        Some(flusher),
        BUDGET,
    ));
    assert_pending(shutdown.as_mut()).await;

    drop(shutdown);
    let destructor_observed = tokio::time::timeout(Duration::from_millis(50), dropped).await;
    cleanup.abort();

    assert!(
        matches!(destructor_observed, Ok(Ok(()))),
        "cancellation must not detach flusher"
    );
}

#[tokio::test]
async fn flusher_timeout_is_reported_without_claiming_complete_shutdown() {
    let (tenants, backend) = persistent_tenant();
    let (flusher, mut dropped) = held_flusher().await;
    let cleanup = flusher.task.as_ref().unwrap().abort_handle();

    let report = shutdown_tenants(
        RuntimeHealthState::new(),
        stop_channel(),
        tenants,
        Some(flusher),
        Duration::from_millis(10),
    )
    .await;
    let destructor_observed = dropped.try_recv();
    cleanup.abort();

    assert!(!report.locally_complete());
    assert_eq!(destructor_observed, Ok(()));
    assert_eq!(
        backend.writes(),
        1,
        "best-effort flush follows joined cancellation"
    );
}

#[tokio::test]
async fn drain_waits_for_admitted_work_before_final_flush() {
    let (tenants, backend) = persistent_tenant();
    let health = RuntimeHealthState::new();
    let lease = health.try_admit().unwrap();
    let (stop, mut stopped) = watch::channel(false);
    let shutdown = shutdown_tenants(health.clone(), stop, tenants, None, BUDGET);
    tokio::pin!(shutdown);

    assert_pending(shutdown.as_mut()).await;
    assert!(*stopped.borrow_and_update());
    assert!(health.try_admit().is_none());
    assert_eq!(backend.writes(), 0);
    drop(lease);

    let report = shutdown.await;
    assert!(report.locally_complete());
    assert_eq!(backend.writes(), 1);
}

#[tokio::test]
async fn flush_failure_and_timeout_remain_incomplete() {
    for pause in [false, true] {
        let (tenants, backend) = persistent_tenant();
        if pause {
            backend.pause_writes();
        } else {
            backend.fail_on(KEY);
        }
        let report = shutdown_tenants(
            RuntimeHealthState::new(),
            stop_channel(),
            tenants,
            None,
            Duration::from_millis(10),
        )
        .await;
        assert!(!report.locally_complete());
        assert_eq!(
            report.flush,
            if pause {
                FinalFlush::TimedOut
            } else {
                FinalFlush::Failed
            }
        );
        assert_eq!(backend.writes(), 0);
    }
}

#[tokio::test]
async fn drain_timeout_cannot_be_reported_as_complete() {
    let (tenants, _backend) = persistent_tenant();
    let health = RuntimeHealthState::new();
    let _lease = health.try_admit().unwrap();

    let report = shutdown_tenants(
        health,
        stop_channel(),
        tenants,
        None,
        Duration::from_millis(10),
    )
    .await;

    assert!(!report.quiesced);
    assert_eq!(report.flush, FinalFlush::Skipped);
    assert!(!report.locally_complete());
}

#[tokio::test]
async fn flusher_failure_is_distinct_from_flush_success() {
    let (tenants, _backend) = persistent_tenant();
    let (stop, _stop_rx) = oneshot::channel();
    let task = tokio::spawn(async { panic!("injected writer panic") });

    let report = shutdown_tenants(
        RuntimeHealthState::new(),
        stop_channel(),
        tenants,
        Some(TenantFlusher::new((stop, task))),
        BUDGET,
    )
    .await;

    assert_eq!(report.flusher, FlusherStop::Failed);
    assert!(matches!(report.flush, FinalFlush::Completed(1)));
    assert!(!report.locally_complete());
}

/// A non-cooperative writer cannot be confirmed joined; final flush must be skipped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unconfirmed_writer_termination_skips_final_flush() {
    let (tenants, backend) = persistent_tenant();
    let (stop, _stop_rx) = oneshot::channel();
    let (entered, ready) = oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let (done, done_rx) = oneshot::channel();
    let task = tokio::task::spawn_blocking(move || {
        entered.send(()).unwrap();
        let _ = blocked.recv();
        let _ = done.send(());
    });
    ready.await.unwrap();
    let report = shutdown_tenants(
        RuntimeHealthState::new(),
        stop_channel(),
        tenants,
        Some(TenantFlusher::new((stop, task))),
        Duration::from_millis(10),
    )
    .await;
    release.send(()).unwrap();
    done_rx.await.unwrap();
    assert_eq!(report.flusher, FlusherStop::TimedOut);
    assert_eq!(report.flush, FinalFlush::Skipped);
    assert_eq!(backend.writes(), 0);
    assert!(!report.locally_complete());
}

#[tokio::test]
async fn normal_flusher_join_precedes_latest_snapshot_flush() {
    let (tenants, backend) = persistent_tenant();
    backend.pause_writes();
    let control = tenants.spawn_flusher(Duration::from_secs(60)).unwrap();
    tokio::time::timeout(BUDGET, backend.wait_until_write_blocked())
        .await
        .unwrap();
    let mut shutdown = Box::pin(shutdown_tenants(
        RuntimeHealthState::new(),
        stop_channel(),
        tenants.clone(),
        Some(TenantFlusher::new(control)),
        BUDGET,
    ));
    assert_pending(shutdown.as_mut()).await;
    tenants.set_balance(KEY, 42.0);
    backend.resume_writes();
    let report = shutdown.await;
    assert!(report.locally_complete());
    assert_eq!(backend.stored(KEY).unwrap()["balance_usd"], "42");
    assert_eq!(backend.writes(), 2);
}

#[tokio::test]
async fn dirty_version_created_during_final_flush_is_incomplete() {
    let (tenants, backend) = persistent_tenant();
    backend.pause_writes();
    let shutdown = tokio::spawn(shutdown_tenants(
        RuntimeHealthState::new(),
        stop_channel(),
        tenants.clone(),
        None,
        BUDGET,
    ));
    tokio::time::timeout(BUDGET, backend.wait_until_write_blocked())
        .await
        .unwrap();
    tenants.set_balance(KEY, 42.0);
    backend.resume_writes();
    let report = shutdown.await.unwrap();
    assert_eq!(report.flush, FinalFlush::Completed(1));
    assert!(report.dirty_remaining);
    assert!(!report.locally_complete());
    assert_eq!(tenants.flush_dirty().await.unwrap(), 1);
    assert_eq!(backend.stored(KEY).unwrap()["balance_usd"], "42");
}

#[tokio::test]
async fn cancelling_during_flusher_join_aborts_owned_writer() {
    let (flusher, dropped) = held_flusher().await;
    let mut stop = Box::pin(flusher.stop(BUDGET));
    assert_pending(stop.as_mut()).await;
    drop(stop);
    tokio::time::timeout(BUDGET, dropped)
        .await
        .unwrap()
        .unwrap();
}
