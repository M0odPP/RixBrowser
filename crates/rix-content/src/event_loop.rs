//! Reactive WHATWG Event Loop with prioritized macrotask queues, strict
//! microtask checkpoints, VSync-driven `requestAnimationFrame`, and exact
//! deadline timers (`setTimeout` / `setInterval`) with ZERO polling.
//!
//! Reference: Spec 06 §1 & Issue #16.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

use rix_js::{HeapHandle, JsValue, JsVm, PropertyAttrs};
use tokio::sync::mpsc;

/// HTML5 timer nesting clamping threshold (WHATWG HTML §8.6).
const NESTED_TIMER_CLAMP_DEPTH: u32 = 5;
const NESTED_TIMER_MIN_DELAY_MS: f64 = 4.0;

/// Strict WHATWG task source priority ordering (`Input` highest, `Idle` lowest).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum TaskPriority {
    Input = 0,
    Rendering = 1,
    Networking = 2,
    Timer = 3,
    Idle = 4,
}

impl TaskPriority {
    const ALL: [Self; 5] = [
        Self::Input,
        Self::Rendering,
        Self::Networking,
        Self::Timer,
        Self::Idle,
    ];
}

/// Callback executed when a macrotask runs on the content thread.
pub enum MacrotaskAction {
    JsCallback {
        func: HeapHandle,
        this_val: JsValue,
        args: Vec<JsValue>,
    },
    HostClosure(Box<dyn FnOnce(&mut JsVm)>),
}

struct Macrotask {
    action: MacrotaskAction,
}

/// External event delivered into the reactive `tokio::select!` event loop.
#[derive(Debug, Clone)]
pub enum ExternalEvent {
    /// Browser compositor VSync signal (drives `requestAnimationFrame` & rendering).
    Vsync { timestamp_ms: f64 },
    /// High-priority user input task payload.
    InputReady,
    /// Network chunk or response ready.
    NetworkReady,
    /// Graceful shutdown signal.
    Shutdown,
}

#[derive(Debug, Clone)]
struct TimerEntry {
    id: u32,
    deadline_ms: f64,
    seq: u64,
    interval_ms: Option<f64>,
    nesting_level: u32,
    callback: HeapHandle,
    args: Vec<JsValue>,
}

impl PartialEq for TimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline_ms.to_bits() == other.deadline_ms.to_bits() && self.seq == other.seq
    }
}

impl Eq for TimerEntry {}

impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Min-heap ordering: earliest deadline (then lowest seq) pops first.
        other
            .deadline_ms
            .total_cmp(&self.deadline_ms)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

struct RafEntry {
    id: u32,
    callback: HeapHandle,
}

/// Shared hook invoked at every microtask checkpoint (e.g. `MutationObserver` delivery).
pub type MicrotaskCheckpointHook = Rc<RefCell<Box<dyn FnMut(&mut JsVm)>>>;
/// Shared callback invoked when a timer, macrotask, or rAF callback throws an uncaught exception.
pub(crate) type UncaughtErrorHook = Rc<dyn Fn(&mut JsVm, rix_js::JsException)>;

/// Core WHATWG Event Loop state shared cleanly between `ContentRuntime` and JS bindings.
pub struct EventLoopState {
    queues: [VecDeque<Macrotask>; 5],
    timers: BinaryHeap<TimerEntry>,
    active_timer_ids: HashSet<u32>,
    cancelled_timers: HashSet<u32>,
    next_timer_id: u32,
    next_seq: u64,
    current_timer_nesting: u32,
    raf_queue: Vec<RafEntry>,
    cancelled_raf: HashSet<u32>,
    next_raf_id: u32,
    start_instant: Instant,
    virtual_offset_ms: f64,
    use_virtual_clock: bool,
    pub tasks_executed: u64,
    pub checkpoints_executed: u64,
    performing_microtask_checkpoint: bool,
    consecutive_net_tasks: u32,
    checkpoint_hooks: Vec<MicrotaskCheckpointHook>,
    pub(crate) on_uncaught_error: Option<UncaughtErrorHook>,
}

impl Default for EventLoopState {
    fn default() -> Self {
        Self::new()
    }
}

impl EventLoopState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            queues: std::array::from_fn(|_| VecDeque::new()),
            timers: BinaryHeap::new(),
            active_timer_ids: HashSet::new(),
            cancelled_timers: HashSet::new(),
            next_timer_id: 1,
            next_seq: 1,
            current_timer_nesting: 0,
            raf_queue: Vec::new(),
            cancelled_raf: HashSet::new(),
            next_raf_id: 1,
            start_instant: Instant::now(),
            virtual_offset_ms: 0.0,
            use_virtual_clock: false,
            tasks_executed: 0,
            checkpoints_executed: 0,
            performing_microtask_checkpoint: false,
            consecutive_net_tasks: 0,
            checkpoint_hooks: Vec::new(),
            on_uncaught_error: None,
        }
    }

    /// Switches this loop to deterministic virtual time (for reproducible unit/WPT tests).
    pub fn enable_virtual_clock(&mut self) {
        self.use_virtual_clock = true;
        self.virtual_offset_ms = 0.0;
    }

    /// Returns monotonic elapsed milliseconds since loop creation.
    #[must_use]
    pub fn now_ms(&self) -> f64 {
        if self.use_virtual_clock {
            self.virtual_offset_ms
        } else {
            self.start_instant.elapsed().as_secs_f64() * 1000.0 + self.virtual_offset_ms
        }
    }

    /// Advances the virtual clock offset by `delta` (for deterministic testing).
    pub fn advance_clock(&mut self, delta: Duration) {
        self.virtual_offset_ms += delta.as_secs_f64() * 1000.0;
    }

    /// Returns the next monotonic timer sequence watermark.
    #[must_use]
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Pops the earliest due timer whose `seq < max_seq`, preserving later-scheduled timers.
    fn pop_due_timer_before_seq(&mut self, max_seq: u64) -> Option<TimerEntry> {
        let now = self.now_ms();
        let mut skipped = Vec::new();
        let mut found = None;
        while let Some(top) = self.timers.peek() {
            if self.cancelled_timers.contains(&top.id) {
                let entry = self.timers.pop().unwrap();
                self.cancelled_timers.remove(&entry.id);
                self.active_timer_ids.remove(&entry.id);
                continue;
            }
            if top.deadline_ms > now {
                break;
            }
            let entry = self.timers.pop().unwrap();
            if entry.seq < max_seq {
                if let Some(interval) = entry.interval_ms {
                    let next_seq = self.next_seq;
                    self.next_seq = self.next_seq.wrapping_add(1);
                    self.timers.push(TimerEntry {
                        id: entry.id,
                        deadline_ms: now + interval.max(NESTED_TIMER_MIN_DELAY_MS),
                        seq: next_seq,
                        interval_ms: Some(interval),
                        nesting_level: entry.nesting_level.saturating_add(1),
                        callback: entry.callback,
                        args: entry.args.clone(),
                    });
                } else {
                    self.active_timer_ids.remove(&entry.id);
                }
                found = Some(entry);
                break;
            }
            skipped.push(entry);
        }
        for s in skipped {
            self.timers.push(s);
        }
        found
    }

    /// Registers a callback executed during every WHATWG microtask checkpoint.
    pub fn add_checkpoint_hook(&mut self, hook: MicrotaskCheckpointHook) {
        self.checkpoint_hooks.push(hook);
    }

    /// Enqueues a macrotask into the specified WHATWG priority queue.
    pub fn post_task(&mut self, priority: TaskPriority, action: MacrotaskAction) {
        self.queues[priority as usize].push_back(Macrotask { action });
    }

    /// Enqueues a host Rust closure at `priority`.
    pub fn post_host_task(&mut self, priority: TaskPriority, f: impl FnOnce(&mut JsVm) + 'static) {
        self.post_task(priority, MacrotaskAction::HostClosure(Box::new(f)));
    }

    /// Schedules a `setTimeout` or `setInterval` timer and returns its timer ID.
    pub fn schedule_timer(
        &mut self,
        callback: HeapHandle,
        raw_delay_ms: f64,
        args: Vec<JsValue>,
        repeating: bool,
    ) -> u32 {
        let mut delay = if raw_delay_ms.is_finite() && raw_delay_ms > 0.0 {
            raw_delay_ms
        } else {
            0.0
        };
        let nesting = self.current_timer_nesting.saturating_add(1);
        if nesting > NESTED_TIMER_CLAMP_DEPTH && delay < NESTED_TIMER_MIN_DELAY_MS {
            delay = NESTED_TIMER_MIN_DELAY_MS;
        }
        let id = self.next_timer_id;
        self.next_timer_id = self.next_timer_id.wrapping_add(1).max(1);
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        let deadline_ms = self.now_ms() + delay;

        self.active_timer_ids.insert(id);
        self.timers.push(TimerEntry {
            id,
            deadline_ms,
            seq,
            interval_ms: repeating.then_some(delay),
            nesting_level: nesting,
            callback,
            args,
        });
        id
    }

    /// Cancels a pending `setTimeout` or `setInterval`.
    pub fn clear_timer(&mut self, id: u32) {
        if self.active_timer_ids.remove(&id) {
            self.cancelled_timers.insert(id);
        }
    }

    /// Queues a `requestAnimationFrame` callback for the next VSync signal.
    pub fn request_animation_frame(&mut self, callback: HeapHandle) -> u32 {
        let id = self.next_raf_id;
        self.next_raf_id = self.next_raf_id.wrapping_add(1).max(1);
        self.raf_queue.push(RafEntry { id, callback });
        id
    }

    /// Cancels a queued `requestAnimationFrame` callback.
    pub fn cancel_animation_frame(&mut self, id: u32) {
        self.cancelled_raf.insert(id);
    }

    /// Returns `true` when at least one `requestAnimationFrame` callback is waiting for VSync.
    #[must_use]
    pub fn has_pending_raf(&self) -> bool {
        self.raf_queue
            .iter()
            .any(|e| !self.cancelled_raf.contains(&e.id))
    }

    /// Returns `true` when any prioritized macrotask is immediately runnable.
    #[must_use]
    pub fn has_queued_macrotasks(&self) -> bool {
        self.queues.iter().any(|q| !q.is_empty())
    }

    /// Purges cancelled timer entries at the top of the min-heap and returns the earliest
    /// active timer deadline in milliseconds (or `None` when idle with zero active timers).
    pub fn next_timer_deadline_ms(&mut self) -> Option<f64> {
        while let Some(top) = self.timers.peek() {
            if self.cancelled_timers.contains(&top.id) || !self.active_timer_ids.contains(&top.id) {
                let dead = self.timers.pop().unwrap();
                self.cancelled_timers.remove(&dead.id);
            } else {
                return Some(top.deadline_ms);
            }
        }
        None
    }

    /// Computes exact sleep duration until the next active timer fires (`None` = sleep indefinitely).
    pub fn time_until_next_timer(&mut self) -> Option<Duration> {
        let deadline = self.next_timer_deadline_ms()?;
        let now = self.now_ms();
        if deadline <= now {
            Some(Duration::ZERO)
        } else {
            Some(Duration::from_secs_f64((deadline - now) / 1000.0))
        }
    }

    /// Collects all JS callback/argument roots held in queues, timers, and rAF entries for GC.
    pub fn collect_roots(&self, out: &mut Vec<JsValue>) {
        for q in &self.queues {
            for task in q {
                if let MacrotaskAction::JsCallback {
                    func,
                    this_val,
                    args,
                } = &task.action
                {
                    out.push(JsValue::from_object(*func));
                    out.push(*this_val);
                    out.extend_from_slice(args);
                }
            }
        }
        for t in &self.timers {
            if self.active_timer_ids.contains(&t.id) {
                out.push(JsValue::from_object(t.callback));
                out.extend_from_slice(&t.args);
            }
        }
        for r in &self.raf_queue {
            if !self.cancelled_raf.contains(&r.id) {
                out.push(JsValue::from_object(r.callback));
            }
        }
    }

    fn pop_highest_priority_task(&mut self) -> Option<Macrotask> {
        for prio in TaskPriority::ALL {
            if let Some(task) = self.queues[prio as usize].pop_front() {
                return Some(task);
            }
        }
        None
    }

    fn pop_due_timer(&mut self) -> Option<TimerEntry> {
        let now = self.now_ms();
        while let Some(top) = self.timers.peek() {
            if self.cancelled_timers.contains(&top.id) || !self.active_timer_ids.contains(&top.id) {
                let dead = self.timers.pop().unwrap();
                self.cancelled_timers.remove(&dead.id);
                continue;
            }
            if top.deadline_ms <= now {
                let entry = self.timers.pop().unwrap();
                if let Some(interval) = entry.interval_ms {
                    // Reschedule repeating `setInterval` cleanly before executing callback.
                    let seq = self.next_seq;
                    self.next_seq = self.next_seq.wrapping_add(1);
                    let next_nesting = entry.nesting_level.saturating_add(1);
                    let clamped = if next_nesting > NESTED_TIMER_CLAMP_DEPTH
                        && interval < NESTED_TIMER_MIN_DELAY_MS
                    {
                        NESTED_TIMER_MIN_DELAY_MS
                    } else {
                        interval
                    };
                    let next_deadline = entry.deadline_ms + clamped;
                    let scheduled = if next_deadline + clamped < now {
                        now
                    } else {
                        next_deadline
                    };
                    self.timers.push(TimerEntry {
                        id: entry.id,
                        deadline_ms: scheduled,
                        seq,
                        interval_ms: Some(interval),
                        nesting_level: next_nesting,
                        callback: entry.callback,
                        args: entry.args.clone(),
                    });
                } else {
                    self.active_timer_ids.remove(&entry.id);
                }
                return Some(entry);
            }
            break;
        }
        None
    }

    fn take_raf_batch(&mut self) -> Vec<RafEntry> {
        let batch = std::mem::take(&mut self.raf_queue);
        let cancelled = std::mem::take(&mut self.cancelled_raf);
        batch
            .into_iter()
            .filter(|e| !cancelled.contains(&e.id))
            .collect()
    }
}

/// Shared handle to the WHATWG [`EventLoopState`].
pub type SharedEventLoop = Rc<RefCell<EventLoopState>>;

/// Performs a strict WHATWG microtask checkpoint: drains `JsVm` microtasks and invokes
/// registered checkpoint hooks (such as `MutationObserver` record delivery) until empty.
pub fn run_microtask_checkpoint(loop_state: &SharedEventLoop, vm: &mut JsVm) {
    // WHATWG HTML §8.1.4.3 step 1: guard against re-entrant microtask checkpoints.
    if loop_state.borrow().performing_microtask_checkpoint {
        return;
    }
    loop_state.borrow_mut().performing_microtask_checkpoint = true;
    loop {
        vm.drain_microtasks();
        let hooks: Vec<MicrotaskCheckpointHook> = loop_state.borrow().checkpoint_hooks.clone();
        for hook in hooks {
            (hook.borrow_mut())(vm);
        }
        loop_state.borrow_mut().checkpoints_executed += 1;
        if !vm.has_pending_microtasks() {
            break;
        }
    }
    loop_state.borrow_mut().performing_microtask_checkpoint = false;
}

/// Executes at most one ready macrotask or due timer (strictly respecting
/// `Input > Rendering > Networking > Timer > Idle`, with fair anti-starvation yielding
/// so self-reposting networking/messaging loops never starve due timers), followed by a
/// strict microtask checkpoint.
///
/// Returns `true` if a task or timer executed.
pub fn step_event_loop_once(loop_state: &SharedEventLoop, vm: &mut JsVm) -> bool {
    // 1. Highest-priority interactive/rendering tasks always run first.
    for prio in [TaskPriority::Input, TaskPriority::Rendering] {
        let task_opt = loop_state.borrow_mut().queues[prio as usize].pop_front();
        if let Some(task) = task_opt {
            loop_state.borrow_mut().consecutive_net_tasks = 0;
            execute_macrotask(loop_state, vm, task);
            return true;
        }
    }

    // 2. Fair anti-starvation check: after 4 consecutive Networking tasks, give a due Timer
    // a turn so self-posting MessagePort/postMessage loops cannot starve `setTimeout(..., 0)`.
    let yield_to_timer = {
        let mut st = loop_state.borrow_mut();
        st.consecutive_net_tasks >= 4
            && (!st.queues[TaskPriority::Timer as usize].is_empty()
                || st.time_until_next_timer() == Some(Duration::ZERO))
    };

    if !yield_to_timer {
        let net_opt = loop_state.borrow_mut().queues[TaskPriority::Networking as usize].pop_front();
        if let Some(task) = net_opt {
            {
                let mut st = loop_state.borrow_mut();
                st.consecutive_net_tasks = st.consecutive_net_tasks.saturating_add(1);
            }
            execute_macrotask(loop_state, vm, task);
            return true;
        }
    }

    // 3. Check explicit `Timer`-priority queued tasks, then due timer heap entries.
    let timer_task = loop_state.borrow_mut().queues[TaskPriority::Timer as usize].pop_front();
    if let Some(task) = timer_task {
        loop_state.borrow_mut().consecutive_net_tasks = 0;
        execute_macrotask(loop_state, vm, task);
        return true;
    }
    let due_timer = loop_state.borrow_mut().pop_due_timer();
    if let Some(timer) = due_timer {
        let prev_nesting = {
            let mut st = loop_state.borrow_mut();
            st.consecutive_net_tasks = 0;
            let prev = st.current_timer_nesting;
            st.current_timer_nesting = timer.nesting_level;
            prev
        };
        let global = JsValue::from_object(vm.global);
        if let Err(ex) = vm.call_value(JsValue::from_object(timer.callback), global, &timer.args) {
            let hook = loop_state.borrow().on_uncaught_error.clone();
            if let Some(hook) = hook {
                hook(vm, ex);
            }
        }
        {
            let mut st = loop_state.borrow_mut();
            st.current_timer_nesting = prev_nesting;
            st.tasks_executed += 1;
        }
        run_microtask_checkpoint(loop_state, vm);
        return true;
    }

    // 4. Lowest-priority `Idle` tasks (or remaining Networking if yield didn't pop a timer).
    let rem_task = loop_state.borrow_mut().pop_highest_priority_task();
    if let Some(task) = rem_task {
        loop_state.borrow_mut().consecutive_net_tasks = 0;
        execute_macrotask(loop_state, vm, task);
        return true;
    }

    false
}

fn execute_macrotask(loop_state: &SharedEventLoop, vm: &mut JsVm, task: Macrotask) {
    match task.action {
        MacrotaskAction::JsCallback {
            func,
            this_val,
            args,
        } => {
            if let Err(ex) = vm.call_value(JsValue::from_object(func), this_val, &args) {
                let hook = loop_state.borrow().on_uncaught_error.clone();
                if let Some(hook) = hook {
                    hook(vm, ex);
                }
            }
        }
        MacrotaskAction::HostClosure(f) => f(vm),
    }
    loop_state.borrow_mut().tasks_executed += 1;
    run_microtask_checkpoint(loop_state, vm);
}

/// Fires a VSync frame signal at `timestamp_ms`, executing the snapshotted
/// `requestAnimationFrame` callback batch followed by a strict microtask checkpoint.
/// Returns the number of rAF callbacks executed (0 when idle!).
pub fn fire_vsync(loop_state: &SharedEventLoop, vm: &mut JsVm, timestamp_ms: f64) -> usize {
    let batch = loop_state.borrow_mut().take_raf_batch();
    let count = batch.len();
    for entry in batch {
        if let Err(ex) = vm.call_value(
            JsValue::from_object(entry.callback),
            JsValue::UNDEFINED,
            &[JsValue::from_f64(timestamp_ms)],
        ) {
            let hook = loop_state.borrow().on_uncaught_error.clone();
            if let Some(hook) = hook {
                hook(vm, ex);
            }
        }
        loop_state.borrow_mut().tasks_executed += 1;
        run_microtask_checkpoint(loop_state, vm);
    }
    count
}

/// Drains due timers queued before `max_seq` (e.g. zero-delay tasks posted by `defer` scripts
/// before the `DOMContentLoaded` task is queued per WHATWG HTML §13.2.7).
pub fn drain_due_timers_before_seq(loop_state: &SharedEventLoop, vm: &mut JsVm, max_seq: u64) {
    const MAX_PRE_DCL_TIMERS: u64 = 256;
    run_microtask_checkpoint(loop_state, vm);
    let mut ran = 0u64;
    while ran < MAX_PRE_DCL_TIMERS {
        let Some(timer) = loop_state.borrow_mut().pop_due_timer_before_seq(max_seq) else {
            break;
        };
        let prev_nesting = {
            let mut st = loop_state.borrow_mut();
            let prev = st.current_timer_nesting;
            st.current_timer_nesting = timer.nesting_level;
            prev
        };
        let global = JsValue::from_object(vm.global);
        if let Err(ex) = vm.call_value(JsValue::from_object(timer.callback), global, &timer.args) {
            let hook = loop_state.borrow().on_uncaught_error.clone();
            if let Some(hook) = hook {
                hook(vm, ex);
            }
        }
        {
            let mut st = loop_state.borrow_mut();
            st.current_timer_nesting = prev_nesting;
            st.tasks_executed += 1;
        }
        run_microtask_checkpoint(loop_state, vm);
        ran += 1;
    }
}

/// Drains immediately ready macrotasks, due timers, and microtasks until quiescent
/// (capped per turn so intentional infinite self-posting loops yield cleanly).
pub fn drain_ready_tasks(loop_state: &SharedEventLoop, vm: &mut JsVm) -> u64 {
    const MAX_TASKS_PER_DRAIN: u64 = 2_000;
    run_microtask_checkpoint(loop_state, vm);
    let mut ran = 0u64;
    while ran < MAX_TASKS_PER_DRAIN && step_event_loop_once(loop_state, vm) {
        ran += 1;
    }
    ran
}

/// Cooperatively drains immediately ready macrotasks, due timers, and microtasks up to
/// `max_tasks` or wall-clock `budget`, then yields cleanly back to rendering/VSync so
/// self-reposting `MessageChannel` / `setTimeout(0)` framework schedulers never block First Paint.
pub fn drain_ready_tasks_with_budget(
    loop_state: &SharedEventLoop,
    vm: &mut JsVm,
    max_tasks: u64,
    budget: Duration,
) -> u64 {
    let start = Instant::now();
    run_microtask_checkpoint(loop_state, vm);
    let mut ran = 0u64;
    while ran < max_tasks && start.elapsed() < budget && step_event_loop_once(loop_state, vm) {
        ran += 1;
    }
    ran
}

/// Reactive async sleep waiting for EITHER the next [`ExternalEvent`] OR the exact
/// deadline of the earliest active timer (ZERO polling, 0% idle CPU when no timers exist!).
pub async fn wait_for_next_event_or_timer(
    loop_state: &SharedEventLoop,
    ext_rx: &mut mpsc::UnboundedReceiver<ExternalEvent>,
) -> Option<ExternalEvent> {
    let sleep_dur = loop_state.borrow_mut().time_until_next_timer();
    match sleep_dur {
        Some(Duration::ZERO) => None,
        Some(dur) => {
            let deadline = tokio::time::Instant::now() + dur;
            tokio::select! {
                biased;
                ev = ext_rx.recv() => ev,
                () = tokio::time::sleep_until(deadline) => None,
            }
        }
        // No active timers: park cleanly on channel with 0% CPU until an external event arrives.
        None => ext_rx.recv().await,
    }
}

/// Installs `setTimeout`, `clearTimeout`, `setInterval`, `clearInterval`,
/// `requestAnimationFrame`, `cancelAnimationFrame`, and `performance.now()` on `vm`.
pub fn install_event_loop_globals(vm: &mut JsVm, loop_state: &SharedEventLoop) {
    let define_global = |vm: &mut JsVm, name: &str, val: JsValue| {
        let k = vm.heap.intern_key(name);
        let g = vm.global;
        vm.heap
            .object_define_data(g, k, val, PropertyAttrs::METHOD, &mut vm.shapes);
    };

    // setTimeout(callback, delay?, ...args)
    let st_loop = Rc::clone(loop_state);
    let set_timeout = vm.alloc_host_fn("setTimeout", move |vm, _this, args| {
        let cb = args.first().copied().unwrap_or(JsValue::UNDEFINED);
        let cb_handle = resolve_timer_callback(vm, cb)?;
        let delay = args
            .get(1)
            .copied()
            .filter(|v| !v.is_undefined())
            .map(|v| vm.to_number(v))
            .transpose()?
            .unwrap_or(0.0);
        let extra = if args.len() > 2 {
            args[2..].to_vec()
        } else {
            Vec::new()
        };
        let id = st_loop
            .borrow_mut()
            .schedule_timer(cb_handle, delay, extra, false);
        Ok(JsValue::from_i32(id as i32))
    });
    define_global(vm, "setTimeout", JsValue::from_object(set_timeout));

    // setInterval(callback, delay?, ...args)
    let si_loop = Rc::clone(loop_state);
    let set_interval = vm.alloc_host_fn("setInterval", move |vm, _this, args| {
        let cb = args.first().copied().unwrap_or(JsValue::UNDEFINED);
        let cb_handle = resolve_timer_callback(vm, cb)?;
        let delay = args
            .get(1)
            .copied()
            .filter(|v| !v.is_undefined())
            .map(|v| vm.to_number(v))
            .transpose()?
            .unwrap_or(0.0);
        let extra = if args.len() > 2 {
            args[2..].to_vec()
        } else {
            Vec::new()
        };
        let id = si_loop
            .borrow_mut()
            .schedule_timer(cb_handle, delay, extra, true);
        Ok(JsValue::from_i32(id as i32))
    });
    define_global(vm, "setInterval", JsValue::from_object(set_interval));

    // clearTimeout / clearInterval
    let ct_loop = Rc::clone(loop_state);
    let clear_timeout = vm.alloc_host_fn("clearTimeout", move |vm, _this, args| {
        if let Some(&v) = args.first() {
            if !v.is_undefined() && !v.is_null() {
                let id = vm.to_u32(v)?;
                ct_loop.borrow_mut().clear_timer(id);
            }
        }
        Ok(JsValue::UNDEFINED)
    });
    define_global(vm, "clearTimeout", JsValue::from_object(clear_timeout));
    define_global(vm, "clearInterval", JsValue::from_object(clear_timeout));

    // requestAnimationFrame / cancelAnimationFrame
    let raf_loop = Rc::clone(loop_state);
    let raf_fn = vm.alloc_host_fn("requestAnimationFrame", move |vm, _this, args| {
        let Some(cb) = args
            .first()
            .copied()
            .and_then(JsValue::as_object)
            .filter(|&h| vm.is_callable_handle(h))
        else {
            return Err(vm.type_error("requestAnimationFrame requires a callback function"));
        };
        let id = raf_loop.borrow_mut().request_animation_frame(cb);
        Ok(JsValue::from_i32(id as i32))
    });
    define_global(vm, "requestAnimationFrame", JsValue::from_object(raf_fn));

    let craf_loop = Rc::clone(loop_state);
    let craf_fn = vm.alloc_host_fn("cancelAnimationFrame", move |vm, _this, args| {
        if let Some(&v) = args.first() {
            if !v.is_undefined() && !v.is_null() {
                let id = vm.to_u32(v)?;
                craf_loop.borrow_mut().cancel_animation_frame(id);
            }
        }
        Ok(JsValue::UNDEFINED)
    });
    define_global(vm, "cancelAnimationFrame", JsValue::from_object(craf_fn));

    // performance.now() + timing marks map
    let perf = vm.alloc_ordinary_object();
    let perf_loop = Rc::clone(loop_state);
    let now_fn = vm.alloc_host_fn("now", move |_vm, _this, _args| {
        Ok(JsValue::from_f64(perf_loop.borrow().now_ms()))
    });
    let k_now = vm.heap.intern_key("now");
    vm.heap.object_define_data(
        perf,
        k_now,
        JsValue::from_object(now_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    let marks = Rc::new(RefCell::new(HashMap::<String, f64>::new()));
    let m1 = Rc::clone(&marks);
    let p_loop2 = Rc::clone(loop_state);
    let mark_fn = vm.alloc_host_fn("mark", move |vm, _, args| {
        let name = vm.value_to_rust_string(args.first().copied().unwrap_or(JsValue::UNDEFINED));
        m1.borrow_mut().insert(name, p_loop2.borrow().now_ms());
        Ok(JsValue::UNDEFINED)
    });
    let k_mark = vm.heap.intern_key("mark");
    vm.heap.object_define_data(
        perf,
        k_mark,
        JsValue::from_object(mark_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    define_global(vm, "performance", JsValue::from_object(perf));
}

fn resolve_timer_callback(vm: &mut JsVm, val: JsValue) -> Result<HeapHandle, rix_js::JsException> {
    if let Some(h) = val.as_object() {
        if vm.is_callable_handle(h) {
            return Ok(h);
        }
    }
    if val.is_string() {
        let src = vm.value_to_rust_string(val);
        let wrapper = vm.alloc_host_fn("timer_eval", move |vm, _, _| {
            vm.eval_script(&src).map_err(|e| match e {
                rix_js::EvalError::Runtime(ex) => ex,
                other => vm.make_error("SyntaxError", other.to_string()),
            })?;
            Ok(JsValue::UNDEFINED)
        });
        return Ok(wrapper);
    }
    Err(vm.type_error("Timer callback must be a function or string"))
}
