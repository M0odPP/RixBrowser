//! Cross-thread W3C Media Source Extensions in Dedicated Workers bridge (`MediaSourceHandle`).
//!
//! Coordinates zero-polling, `#![deny(unsafe_code)]` reactive synchronization between a
//! `DedicatedWorkerGlobalScope` owning a `MediaSource` / `ManagedMediaSource` and a main-thread
//! `HTMLMediaElement` attached via `video.srcObject = handle`.
//!
//! Reference: W3C Media Source Extensions §2.3 (`MediaSourceHandle`) & §8 (Worker Attachment).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use rix_dom::NodeId;
use rix_media::{EndOfStreamError, ReadyState, TimeRanges};
use tokio::sync::mpsc;

use crate::workers::AsyncWorkerEvent;

static NEXT_WORKER_MSE_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_worker_mse_id() -> u64 {
    NEXT_WORKER_MSE_ID.fetch_add(1, Ordering::Relaxed)
}

/// Thread-safe snapshot of a worker-owned `MediaSourceEngine` mirrored for main-thread
/// `HTMLMediaElement` reads (`duration`, `buffered`, `seekable`, `readyState`, playback stepping).
#[derive(Debug, Clone)]
pub(crate) struct WorkerMseSnapshot {
    pub ready_state: ReadyState,
    pub duration: f64,
    pub buffered: TimeRanges,
    pub seekable: TimeRanges,
    pub all_inits_received: bool,
    pub first_video_dims: Option<(u32, u32)>,
    pub current_playback_time_s: f64,
}

impl Default for WorkerMseSnapshot {
    fn default() -> Self {
        Self {
            ready_state: ReadyState::Closed,
            duration: f64::NAN,
            buffered: TimeRanges::new(),
            seekable: TimeRanges::new(),
            all_inits_received: false,
            first_video_dims: None,
            current_playback_time_s: 0.0,
        }
    }
}

/// Local JS `MediaSourceHandle` state inside either a `DedicatedWorker` or `Window` realm.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MediaSourceHandleState {
    pub bridge_id: u64,
    /// Set to `true` once transferred via `postMessage(..., [handle])`.
    pub detached: bool,
    /// Set to `true` synchronously when assigned to `HTMLMediaElement.srcObject` (even if immediately cleared).
    pub has_been_attached: bool,
}

/// Shared cross-thread state for one worker-created `MediaSource` / `ManagedMediaSource`.
pub(crate) struct WorkerMseBridge {
    pub worker_alive: bool,
    pub worker_tx: mpsc::UnboundedSender<AsyncWorkerEvent>,
    /// Currently attached main-thread `<video>` element and its reactive event sender.
    pub attached_elem: Option<(NodeId, mpsc::UnboundedSender<AsyncWorkerEvent>)>,
    /// Atomic single-attachment guard per W3C MSE-in-Workers: when the same handle is assigned
    /// synchronously to multiple `<video>` elements, precisely one load succeeds.
    pub has_ever_attached: bool,
    pub snapshot: WorkerMseSnapshot,
}

type WorkerMseRegistryMap = HashMap<u64, Arc<Mutex<WorkerMseBridge>>>;
static WORKER_MSE_REGISTRY: OnceLock<Mutex<WorkerMseRegistryMap>> = OnceLock::new();

fn registry() -> &'static Mutex<WorkerMseRegistryMap> {
    WORKER_MSE_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Registers a newly created worker `MediaSource` in the process-wide bridge registry.
pub(crate) fn register_worker_mse_bridge(
    worker_tx: mpsc::UnboundedSender<AsyncWorkerEvent>,
) -> u64 {
    let id = next_worker_mse_id();
    let bridge = Arc::new(Mutex::new(WorkerMseBridge {
        worker_alive: true,
        worker_tx,
        attached_elem: None,
        has_ever_attached: false,
        snapshot: WorkerMseSnapshot::default(),
    }));
    if let Ok(mut reg) = registry().lock() {
        reg.insert(id, bridge);
    }
    id
}

/// Looks up a shared worker MSE bridge by ID.
pub(crate) fn lookup_worker_mse_bridge(id: u64) -> Option<Arc<Mutex<WorkerMseBridge>>> {
    registry().lock().ok()?.get(&id).cloned()
}

/// Atomically claims attachment of `bridge_id` for main-thread element `elem_id`.
/// Returns `true` and notifies the worker thread if attachment succeeded; returns `false`
/// if the worker has already terminated or the handle was already attached elsewhere.
pub(crate) fn try_claim_bridge_attachment(
    bridge_id: u64,
    elem_id: NodeId,
    main_tx: mpsc::UnboundedSender<AsyncWorkerEvent>,
) -> bool {
    let Some(bridge) = lookup_worker_mse_bridge(bridge_id) else {
        return false;
    };
    let worker_tx = {
        let Ok(mut b) = bridge.lock() else {
            return false;
        };
        if !b.worker_alive || b.attached_elem.is_some() || b.has_ever_attached {
            return false;
        }
        b.attached_elem = Some((elem_id, main_tx));
        b.has_ever_attached = true;
        b.worker_tx.clone()
    };
    worker_tx
        .send(AsyncWorkerEvent::WorkerMseAttach { bridge_id })
        .is_ok()
}

/// Detaches `elem_id` from `bridge_id` (if still the active owner) and notifies the worker thread
/// so the worker's `MediaSource` transitions to `"closed"` and fires `"sourceclose"`.
pub(crate) fn detach_bridge_attachment(bridge_id: u64, elem_id: NodeId) {
    let Some(bridge) = lookup_worker_mse_bridge(bridge_id) else {
        return;
    };
    let worker_tx = {
        let Ok(mut b) = bridge.lock() else {
            return;
        };
        let is_owner = b.attached_elem.as_ref().is_some_and(|(n, _)| *n == elem_id);
        if !is_owner {
            return;
        }
        b.attached_elem = None;
        b.snapshot = WorkerMseSnapshot::default();
        if b.worker_alive {
            Some(b.worker_tx.clone())
        } else {
            None
        }
    };
    if let Some(tx) = worker_tx {
        let _ = tx.send(AsyncWorkerEvent::WorkerMseDetach { bridge_id });
    }
}

/// Marks a worker-owned bridge as dead when its owning `DedicatedWorker` terminates,
/// notifying any currently attached main-thread `<video>` element reactively.
pub(crate) fn mark_worker_mse_dead(bridge_id: u64) {
    let Some(bridge) = lookup_worker_mse_bridge(bridge_id) else {
        return;
    };
    let notify = {
        let Ok(mut b) = bridge.lock() else {
            return;
        };
        b.worker_alive = false;
        b.attached_elem.as_ref().map(|(nid, tx)| (*nid, tx.clone()))
    };
    if let Some((elem_id, main_tx)) = notify {
        let _ = main_tx.send(AsyncWorkerEvent::MainMseWorkerTerminated { bridge_id, elem_id });
    }
    if let Ok(mut reg) = registry().lock() {
        // Keep entry if still referenced by an attached element so snapshot reads stay safe
        // until the element detaches; otherwise clean up immediately.
        let still_attached = bridge
            .lock()
            .ok()
            .is_some_and(|b| b.attached_elem.is_some());
        if !still_attached {
            reg.remove(&bridge_id);
        }
    }
}

/// Pushes an updated worker `MediaSourceEngine` snapshot and wakes the attached main-thread element.
#[allow(clippy::too_many_arguments)]
pub(crate) fn push_worker_mse_snapshot(
    bridge_id: u64,
    snapshot: WorkerMseSnapshot,
    duration_changed: bool,
    eos_error: Option<EndOfStreamError>,
) {
    let Some(bridge) = lookup_worker_mse_bridge(bridge_id) else {
        return;
    };
    let notify = {
        let Ok(mut b) = bridge.lock() else {
            return;
        };
        let cur_t = b.snapshot.current_playback_time_s;
        b.snapshot = snapshot;
        b.snapshot.current_playback_time_s = cur_t;
        b.attached_elem.as_ref().map(|(nid, tx)| (*nid, tx.clone()))
    };
    if let Some((elem_id, main_tx)) = notify {
        let _ = main_tx.send(AsyncWorkerEvent::MainMseSync {
            bridge_id,
            elem_id,
            duration_changed,
            eos_error,
        });
    }
}

/// Reads a snapshot of `bridge_id` for main-thread `HTMLMediaElement` properties/playback.
pub(crate) fn read_bridge_snapshot(bridge_id: u64) -> Option<WorkerMseSnapshot> {
    let bridge = lookup_worker_mse_bridge(bridge_id)?;
    let b = bridge.lock().ok()?;
    Some(b.snapshot.clone())
}

/// Updates the mirrored playback head (`currentTime`) on `bridge_id` from main-thread playback.
pub(crate) fn set_bridge_playback_time(bridge_id: u64, time_s: f64) {
    if let Some(bridge) = lookup_worker_mse_bridge(bridge_id) {
        if let Ok(mut b) = bridge.lock() {
            b.snapshot.current_playback_time_s = time_s;
        }
    }
}
