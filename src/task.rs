use std::cell::{Cell, RefCell, UnsafeCell};
use std::collections::VecDeque;
use std::rc::Weak;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crossbeam_queue::SegQueue;
use futures_util::future::LocalBoxFuture;
use futures_util::task::ArcWake;

use crate::driver::{AnyDriver, AnyInterruptor};

pub struct Task {
    pub future: RefCell<Option<LocalBoxFuture<'static, ()>>>,
    pub queue: Weak<UnsafeCell<VecDeque<Arc<Task>>>>,
    pub remote_queue: std::sync::Weak<SegQueue<usize>>,
    pub interruptor: AnyInterruptor,
    pub queued: AtomicBool,
    pub thread_id: std::thread::ThreadId,
    pub token: usize,
    pub waiting: std::sync::Weak<AtomicBool>,
}

impl ArcWake for Task {
    #[inline]
    fn wake_by_ref(arc_self: &Arc<Self>) {
        Task::enqueue_if_needed(arc_self);
    }
}

impl Task {
    #[inline]
    fn enqueue_if_needed(task: &Arc<Self>) {
        if std::thread::current().id() == task.thread_id {
            if !task.queued.swap(true, Ordering::Relaxed) {
                if let Some(queue) = task.queue.upgrade() {
                    // SAFETY: the runtime is single-threaded and only mutates the ready
                    // queue from that thread. We also never hold a mutable queue borrow
                    // while polling task futures, so re-entrant wakes do not alias.
                    unsafe {
                        (&mut *queue.get()).push_back(Arc::clone(task));
                    }
                }
            }
            return;
        }

        if !task.queued.swap(true, Ordering::Relaxed) {
            if let Some(remote_queue) = task.remote_queue.upgrade() {
                remote_queue.push(task.token);
            }
        }

        // Interrupt the driver if it's waiting
        if task
            .waiting
            .upgrade()
            .map_or(false, |waiting| waiting.load(Ordering::Relaxed))
        {
            // Interrupt the driver if the waker is not on the same thread as the runtime
            task.interruptor.interrupt();
        }
    }

    #[inline]
    pub fn mark_dequeued(&self) {
        self.queued.store(false, Ordering::Relaxed);
    }
}

unsafe impl Send for Task {}
unsafe impl Sync for Task {}
