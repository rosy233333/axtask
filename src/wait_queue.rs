use alloc::collections::VecDeque;
use alloc::sync::Arc;
use spinlock::SpinNoIrq;
use crate::{
    AxRunQueue, AxTaskRef, RUN_QUEUE,
};

/// A queue to store sleeping tasks.
///
/// # Examples
///
/// ```
/// use axtask::WaitQueue;
/// use core::sync::atomic::{AtomicU32, Ordering};
///
/// static VALUE: AtomicU32 = AtomicU32::new(0);
/// static WQ: WaitQueue = WaitQueue::new();
///
/// axtask::init_scheduler();
/// // spawn a new task that updates `VALUE` and notifies the main task
/// axtask::spawn(|| {
///     assert_eq!(VALUE.load(Ordering::Relaxed), 0);
///     VALUE.fetch_add(1, Ordering::Relaxed);
///     WQ.notify_one(true); // wake up the main task
/// });
///
/// WQ.wait(); // block until `notify()` is called
/// assert_eq!(VALUE.load(Ordering::Relaxed), 1);
/// ```
pub struct WaitQueue {
    // Support queue lock by external caller,use SpinNoIrq
    // Arceos SpinNoirq current implementation implies irq_save,
    // so it can be nested
    queue: SpinNoIrq<VecDeque<AxTaskRef>>,
}

impl WaitQueue {
    /// Creates an empty wait queue.
    pub const fn new() -> Self {
        Self {
            queue: SpinNoIrq::new(VecDeque::new()),
        }
    }

    /// Creates an empty wait queue with space for at least `capacity` elements.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            queue: SpinNoIrq::new(VecDeque::with_capacity(capacity)),
        }
    }


    /// Blocks the current task and put it into the wait queue, until other task
    /// notifies it.
    pub fn wait(&self) {
        RUN_QUEUE.lock().block_current(|task| {
            self.queue.lock().push_back(task)
        });

        let curr = crate::current();
        self.queue.lock().retain(|t| !curr.ptr_eq(t));
    }

    /// Blocks the current task and put it into the wait queue, until the given
    /// `condition` becomes true.
    ///
    /// Note that even other tasks notify this task, it will not wake up until
    /// the condition becomes true.
    pub fn wait_until<F>(&self, condition: F)
    where
        F: Fn() -> bool,
    {
        loop {
            let mut rq = RUN_QUEUE.lock();
            if condition() {
                break;
            }
            rq.block_current(|task| {
                self.queue.lock().push_back(task);
            });
        }

        let curr = crate::current();
        self.queue.lock().retain(|t| !curr.ptr_eq(t));
    }

    /// Blocks the current task and put it into the wait queue, until other tasks
    /// notify it, or the given duration has elapsed.
    #[cfg(feature = "irq")]
    pub fn wait_timeout(&self, dur: core::time::Duration) -> bool {
        let curr = crate::current();
        let deadline = axhal::time::current_time() + dur;
        debug!(
            "task wait_timeout: {} deadline={:?}",
            curr.id_name(),
            deadline
        );

        // Here, when task has added in timer-list
        // it may be wakeup before task really enter block state
        // if it happend, It will likely never be executed 
        // so timer add after task into block
        RUN_QUEUE.lock().block_current(|task| {
            self.queue.lock().push_back(task);
            crate::timers::set_alarm_wakeup(deadline, curr.clone());
        });

        // taska may be wake up by: notify timer and signal
        // like linux, no additional waiting queue or timer queue is required.
        // To determine wheather task is timeout,
        // only check whether the time has expired.
        let timeout = axhal::time::current_time() > deadline;

        crate::timers::cancel_alarm(curr.as_task_ref());
        self.queue.lock().retain(|t| !curr.ptr_eq(t));
        timeout
    }

    /// Blocks the current task and put it into the wait queue, until the given
    /// `condition` becomes true, or the given duration has elapsed.
    ///
    /// Note that even other tasks notify this task, it will not wake up until
    /// the above conditions are met.
    #[cfg(feature = "irq")]
    pub fn wait_timeout_until<F>(&self, dur: core::time::Duration, condition: F) -> bool
    where
        F: Fn() -> bool,
    {
        let curr = crate::current();
        let deadline = axhal::time::current_time() + dur;
        debug!(
            "task wait_timeout: {}, deadline={:?}",
            curr.id_name(),
            deadline
        );
        crate::timers::set_alarm_wakeup(deadline, curr.clone());

        let mut timeout = true;
        while axhal::time::current_time() < deadline {
            let mut rq = RUN_QUEUE.lock();
            if condition() {
                timeout = false;
                break;
            }
            rq.block_current(|task| {
                self.queue.lock().push_back(task);
            });
        }
        crate::timers::cancel_alarm(curr.as_task_ref());
        self.queue.lock().retain(|t| !curr.ptr_eq(t));
        timeout
    }

    /// Wakes up one task in the wait queue, usually the first one.
    ///
    /// If `resched` is true, the current task will be preempted when the
    /// preemption is enabled.
    pub fn notify_one(&self, resched: bool) -> bool {
        let mut rq = RUN_QUEUE.lock();
        if !self.queue.lock().is_empty() {
            self.notify_one_locked(resched, &mut rq)
        } else {
            false
        }
    }

    /// Wakes all tasks in the wait queue.
    ///
    /// If `resched` is true, the current task will be preempted when the
    /// preemption is enabled.
    pub fn notify_all(&self, resched: bool) {
        loop {
            let mut rq = RUN_QUEUE.lock();
            if let Some(task) = self.queue.lock().pop_front() {
                rq.unblock_task(task, resched);
            } else {
                break;
            }
            drop(rq); // we must unlock `RUN_QUEUE` after unlocking `self.queue`.
        }
    }

    /// Wake up the given task in the wait queue.
    ///
    /// If `resched` is true, the current task will be preempted when the
    /// preemption is enabled.
    pub fn notify_task(&self, resched: bool, task: &AxTaskRef) -> bool {
        let mut rq = RUN_QUEUE.lock();
        let mut wq = self.queue.lock();
        if let Some(index) = wq.iter().position(|t| Arc::ptr_eq(t, task)) {
            rq.unblock_task(wq.remove(index).unwrap(), resched);
            true
        } else {
            false
        }
    }

    pub(crate) fn notify_one_locked(&self, resched: bool, rq: &mut AxRunQueue) -> bool {
        if let Some(task) = self.queue.lock().pop_front() {
            rq.unblock_task(task, resched);
            true
        } else {
            false
        }
    }

    pub(crate) fn notify_all_locked(&self, resched: bool, rq: &mut AxRunQueue) {
        while let Some(task) = self.queue.lock().pop_front() {
            rq.unblock_task(task, resched);
        }
    }
}
