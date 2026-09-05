use super::*;
use std::future::Future;

fn spawn(id: u64) -> SubagentSpawn {
    SubagentSpawn {
        id,
        parent_id: None,
        depth: 0,
        call_id: format!("call-{id}"),
        task: format!("task-{id}"),
        identity: None,
    }
}

#[tokio::test]
async fn cancel_all_preserves_live_capacity_across_notification_generations() {
    let manager = SubagentManager::new(2);
    let inner = &manager.inner;
    let (old_generation, old_cancel, first) = inner.admit(&spawn(1)).unwrap().into_parts();
    let (_, _, second) = inner.admit(&spawn(2)).unwrap().into_parts();
    assert!(first.is_some() && second.is_some());
    assert_eq!(manager.cancel_all(), 2);
    assert!(old_cancel.is_cancelled());
    assert!(!manager.is_current(old_generation));

    let (generation, cancellation, replacement) = inner.admit(&spawn(3)).unwrap().into_parts();
    assert!(manager.is_current(generation));
    assert!(
        replacement.is_none(),
        "cancelled workers still hold capacity"
    );

    drop(first);
    let replacement = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        inner.acquire(3, &cancellation),
    )
    .await
    .expect("old worker exit must wake the new generation")
    .expect("replacement must receive the released slot");
    // The other old worker exits after a replacement has already started.
    drop(second);
    let (_, _, fourth) = inner.admit(&spawn(4)).unwrap().into_parts();
    assert!(fourth.is_some());
    let (_, _, fifth) = inner.admit(&spawn(5)).unwrap().into_parts();
    assert!(
        fifth.is_none(),
        "old drops must not free new workers' slots"
    );
    assert_eq!(inner.state.lock().unwrap().running, 2);

    assert_eq!(manager.cancel_all(), 3);
    let (_, final_cancel, final_slot) = inner.admit(&spawn(6)).unwrap().into_parts();
    assert!(final_slot.is_none());
    drop(replacement);
    drop(fourth);
    let final_slot = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        inner.acquire(6, &final_cancel),
    )
    .await
    .expect("repeated cancellation must not leak capacity")
    .unwrap();
    drop(final_slot);
    assert_eq!(inner.state.lock().unwrap().running, 0);
}

#[tokio::test]
async fn cancel_all_wakes_queued_acquisition_without_a_slot_release() {
    let manager = SubagentManager::new(1);
    let (_, _, running) = manager.inner.admit(&spawn(1)).unwrap().into_parts();
    let (_, cancellation, queued) = manager.inner.admit(&spawn(2)).unwrap().into_parts();
    assert!(queued.is_none());
    let waiting = manager.inner.acquire(2, &cancellation);
    tokio::pin!(waiting);
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(waiting.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    manager.cancel_all();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
            .await
            .expect("cancellation must wake queued workers even while a slot is held")
            .is_none()
    );
    drop(running);
}

#[test]
fn completion_capacity_is_released_by_consumption_or_cancellation() {
    let manager = SubagentManager::new(0).with_completion_capacity(1);
    let (generation, _, slot) = manager.inner.admit(&spawn(1)).unwrap().into_parts();
    assert!(manager.inner.admit(&spawn(2)).is_err());
    drop(slot);
    manager.inner.finish(generation, 1);
    assert!(manager.active().is_empty());
    assert!(
        manager.inner.admit(&spawn(2)).is_err(),
        "finished results still reserve delivery capacity"
    );
    manager.acknowledge(generation.wrapping_add(1), 1);
    assert!(
        manager.inner.admit(&spawn(2)).is_err(),
        "stale acknowledgements must not release reservations"
    );
    manager.acknowledge(generation, 1);
    let (_, _, slot) = manager.inner.admit(&spawn(2)).unwrap().into_parts();
    drop(slot);
    manager.cancel_all();
    assert!(manager.inner.admit(&spawn(3)).is_ok());
}

#[test]
fn abandoned_admission_releases_registration_and_completion_capacity() {
    let manager = SubagentManager::new(1).with_completion_capacity(1);
    let admission = manager.inner.admit(&spawn(1)).unwrap();
    assert!(manager.inner.admit(&spawn(2)).is_err());
    drop(admission);
    assert!(manager.active().is_empty());
    let replacement = manager.inner.admit(&spawn(2)).unwrap();
    assert!(replacement.slot.is_some());
}
