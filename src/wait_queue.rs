#[cfg(feature = "irq")]
use crate::schedule::schedule_timeout;
use crate::schedule::{schedule, wakeup_task};
use crate::task::TaskState;
use crate::AxTaskRef;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use spinlock::SpinNoIrq;

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
///
///
struct Waiter {
    task: AxTaskRef,
    in_wait: AtomicBool,
}

impl Waiter {
    /// Creates a waiter.
    fn new(task: AxTaskRef) -> Arc<Self> {
        Arc::new(Waiter {
            in_wait: AtomicBool::new(false),
            task: task,
        })
    }

    fn in_wait(&self) -> bool {
        self.in_wait.load(Ordering::Acquire)
    }

    fn set_in_wait(&self, val: bool) {
        self.in_wait.store(val, Ordering::Relaxed)
    }
}

pub struct WaitQueue {
    // Support queue lock by external caller,use SpinNoIrq
    // Arceos SpinNoirq current implementation implies irq_save,
    // so it can be nested
    // TODO: use linked list has good performance
    queue: SpinNoIrq<VecDeque<Arc<Waiter>>>,
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

    // CPU0 wait                CPU1 notify           CPU2 signal
    // q.lock()
    // task.lock()
    // task.state = blocking
    // task.unlock()
    // q.unlock()
    //                          q.lock()
    //                          task = q.get;
    //                          wakeup(task)
    //                            task == blocking
    //                              task = runable
    //                          q.unlock()
    // schedule()
    // queue.lock().remove(curr)
    //
    /// Blocks the current task and put it into the wait queue, until other task
    /// notifies it.
    pub fn wait(&self) {
        let curr = crate::current();

        let mut queue = self.queue.lock();
        curr.set_state(TaskState::Blocking);
        let waiter = Waiter::new(curr.clone());
        waiter.set_in_wait(true);
        queue.push_back(waiter.clone());
        drop(queue);

        schedule();

        // maybe wakeup by signal or others, try to delete again
        // TODO: 
        // 1. starry support UNINTERRUPT mask, no need to check
        // 2. starry support INTERRUPTABLE mask, still need to check
        let mut queue = self.queue.lock();
        if waiter.in_wait() {
            queue.retain(|t| !Arc::<Waiter>::ptr_eq(&waiter, t));
        }
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
        let curr = crate::current();
        let waiter = Waiter::new(curr.clone());

        loop {
            if condition() {
                break;
            }
            let mut queue = self.queue.lock();
            curr.set_state(TaskState::Blocking);
            //maybe wakeup by signal or others, should check before push
            if !waiter.in_wait() {
                waiter.set_in_wait(true);
                queue.push_back(waiter.clone());
            }
            drop(queue);
            schedule();
        }

        //maybe wakeup by signal or others, try to delete again
        let mut queue = self.queue.lock();
        if waiter.in_wait() {
            queue.retain(|t| !Arc::<Waiter>::ptr_eq(&waiter, t));
        }
    }

    /// Blocks the current task and put it into the wait queue, until other tasks
    /// notify it, or the given duration has elapsed.
    #[cfg(feature = "irq")]
    pub fn wait_timeout(&self, dur: core::time::Duration) -> bool {
        let curr = crate::current();
        let waiter = Waiter::new(curr.clone());

        let deadline = axhal::time::current_time() + dur;
        error!(
            "task wait_timeout: {} deadline={:?}",
            curr.id_name(),
            deadline
        );

        let mut queue = self.queue.lock();
        curr.set_state(TaskState::Blocking);
        waiter.set_in_wait(true);
        queue.push_back(waiter.clone());
        drop(queue);

        let timeout = schedule_timeout(deadline);

        //maybe wakeup by timer or signal, try to delete again
        let mut queue = self.queue.lock();
        if waiter.in_wait() {
            queue.retain(|t| !Arc::<Waiter>::ptr_eq(&waiter, t));
        }
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
        let waiter = Waiter::new(curr.clone());

        let deadline = axhal::time::current_time() + dur;
        debug!(
            "task wait_timeout: {}, deadline={:?}",
            curr.id_name(),
            deadline
        );

        let mut timeout = false;
        loop {
            if condition() {
                break;
            }
            let mut queue = self.queue.lock();
            curr.set_state(TaskState::Blocking);
            //maybe wakeup by signal or others, should check before push
            if !waiter.in_wait() {
                waiter.set_in_wait(true);
                queue.push_back(waiter.clone());
            }
            drop(queue);

            timeout = schedule_timeout(deadline);
            if timeout {
                break;
            }
        }

        //maybe wakeup by timer or signal, try to delete again
        let mut queue = self.queue.lock();
        if waiter.in_wait() {
            queue.retain(|t| !Arc::<Waiter>::ptr_eq(&waiter, t));
        }
        timeout
    }

    /// Wake up the given task in the wait queue.
    pub fn notify_task(&self, task: &AxTaskRef) -> bool {
        let mut wq = self.queue.lock();

        if let Some(index) = wq.iter().position(|t| Arc::ptr_eq(&t.task, task)) {
            let waiter = wq.remove(index).unwrap();
            waiter.set_in_wait(false);
            wakeup_task(waiter.task.clone());
            true
        } else {
            false
        }
    }

    /// Wakes up one task in the wait queue, usually the first one.
    pub fn notify_one(&self) -> bool {
        let mut queue = self.queue.lock();
        if let Some(waiter) = queue.pop_front() {
            waiter.set_in_wait(false);
            wakeup_task(waiter.task.clone());
            return true;
        }
        false
    }

    /// Wakes all tasks in the wait queue.
    pub fn notify_all(&self) {
        loop {
            if !self.notify_one() {
                break;
            }
        }
    }
}
