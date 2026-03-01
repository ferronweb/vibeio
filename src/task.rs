use std::cell::{Cell, RefCell, UnsafeCell};
use std::collections::{LinkedList, VecDeque};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{RawWaker, RawWakerVTable, Waker};

use crossbeam_queue::SegQueue;
use futures_util::future::LocalBoxFuture;

use crate::driver::AnyDriver;

pub struct Task {
    pub future: RefCell<Option<LocalBoxFuture<'static, ()>>>,
    pub queue: Weak<SegQueue<Rc<Task>>>,
    pub driver: Weak<AnyDriver>,
    pub queued: Cell<bool>,
    pub thread_id: std::thread::ThreadId,
    pub waiting: Weak<AtomicBool>,
}

impl Task {
    #[inline]
    pub fn waker(self: &Rc<Self>) -> Waker {
        // SAFETY: the vtable methods correctly clone/drop the Rc reference count.
        unsafe { Waker::from_raw(Self::raw_waker(Rc::into_raw(Rc::clone(self)) as *const ())) }
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

    unsafe fn raw_waker_clone(ptr: *const ()) -> RawWaker {
        let task = Rc::<Self>::from_raw(ptr as *const Self);
        let cloned = Rc::clone(&task);
        let _ = Rc::into_raw(task);
        Self::raw_waker(Rc::into_raw(cloned) as *const ())
    }

    unsafe fn raw_waker_wake(ptr: *const ()) {
        let task = Rc::<Self>::from_raw(ptr as *const Self);
        Self::enqueue_if_needed(&task);
    }

    unsafe fn raw_waker_wake_by_ref(ptr: *const ()) {
        let task = Rc::<Self>::from_raw(ptr as *const Self);
        Self::enqueue_if_needed(&task);
        let _ = Rc::into_raw(task);
    }

    unsafe fn raw_waker_drop(ptr: *const ()) {
        drop(Rc::<Self>::from_raw(ptr as *const Self));
    }

    #[inline]
    fn enqueue_if_needed(task: &Rc<Self>) {
        if !task.queued.replace(true) {
            if let Some(queue) = task.queue.upgrade() {
                queue.push(Rc::clone(task));
            }
        }

        if std::thread::current().id() != task.thread_id {
            // Interrupt the driver if it's waiting
            if task
                .waiting
                .upgrade()
                .map_or(false, |waiting| waiting.load(Ordering::Relaxed))
            {
                // Interrupt the driver if the waker is not on the same thread as the runtime
                if let Some(driver) = task.driver.upgrade() {
                    driver.interrupt();
                }
            }
        }
    }

    #[inline]
    pub fn mark_dequeued(&self) {
        self.queued.set(false);
    }
}
