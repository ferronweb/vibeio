use std::cell::{RefCell, UnsafeCell};
use std::collections::VecDeque;
use std::rc::Weak;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{RawWaker, RawWakerVTable, Waker};

use futures_util::future::LocalBoxFuture;

use crate::executor::RuntimeShared;

pub struct Task {
    pub future: RefCell<Option<LocalBoxFuture<'static, ()>>>,
    pub queue: Weak<UnsafeCell<VecDeque<Arc<Task>>>>,
    pub next_task: Weak<RefCell<Option<Arc<Task>>>>,
    pub runtime: std::sync::Weak<RuntimeShared>,
    pub queued: AtomicBool,
    pub thread_id: std::thread::ThreadId,
    pub token: usize,
}

impl Task {
    #[inline]
    pub fn waker(self: &Arc<Self>) -> Waker {
        // SAFETY: the vtable methods correctly clone/drop the Arc reference count.
        unsafe { Waker::from_raw(Self::raw_waker(Arc::into_raw(Arc::clone(self)) as *const ())) }
    }

    #[inline]
    unsafe fn raw_waker(ptr: *const ()) -> RawWaker {
        RawWaker::new(ptr, &Self::VTABLE)
    }

    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        Self::raw_waker_clone,
        Self::raw_waker_wake,
        Self::raw_waker_wake_by_ref,
        Self::raw_waker_drop,
    );

    #[inline]
    unsafe fn raw_waker_clone(ptr: *const ()) -> RawWaker {
        let task = Arc::<Self>::from_raw(ptr as *const Self);
        let cloned = Arc::clone(&task);
        let _ = Arc::into_raw(task);
        Self::raw_waker(Arc::into_raw(cloned) as *const ())
    }

    #[inline]
    unsafe fn raw_waker_wake(ptr: *const ()) {
        let task = Arc::<Self>::from_raw(ptr as *const Self);
        Self::enqueue_if_needed(&task);
    }

    #[inline]
    unsafe fn raw_waker_wake_by_ref(ptr: *const ()) {
        let task = Arc::<Self>::from_raw(ptr as *const Self);
        Self::enqueue_if_needed(&task);
        let _ = Arc::into_raw(task);
    }

    #[inline]
    unsafe fn raw_waker_drop(ptr: *const ()) {
        drop(Arc::<Self>::from_raw(ptr as *const Self));
    }

    #[inline]
    fn enqueue_if_needed(task: &Arc<Self>) {
        if std::thread::current().id() == task.thread_id {
            if !task.queued.swap(true, Ordering::Relaxed) {
                let mut pushed_next = false;
                if let Some(next_task) = task.next_task.upgrade() {
                    let mut next_task = next_task.borrow_mut();
                    if next_task.is_none() {
                        *next_task = Some(Arc::clone(task));
                        pushed_next = true;
                    }
                }
                if !pushed_next {
                    if let Some(queue) = task.queue.upgrade() {
                        // SAFETY: the runtime is single-threaded and only mutates the ready
                        // queue from that thread. We also never hold a mutable queue borrow
                        // while polling task futures, so re-entrant wakes do not alias.
                        unsafe {
                            (&mut *queue.get()).push_back(Arc::clone(task));
                        }
                    }
                }
            }
            return;
        }

        // Cross-thread wake path
        if let Some(shared) = task.runtime.upgrade() {
            if !task.queued.swap(true, Ordering::Relaxed) {
                shared.remote_queue.push(task.token);
            }

            // Interrupt the driver if it's waiting
            if shared.waiting.load(Ordering::Acquire)
                && !shared.interrupt_pending.swap(true, Ordering::AcqRel)
            {
                shared.interruptor.interrupt();
            }
        }
    }

    #[inline]
    pub fn mark_dequeued(&self) {
        self.queued.store(false, Ordering::Relaxed);
    }
}
