use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::task::{RawWaker, RawWakerVTable, Waker};

use futures_util::future::LocalBoxFuture;

pub struct Task {
    pub future: RefCell<Option<LocalBoxFuture<'static, ()>>>,
    pub queue: Rc<RefCell<VecDeque<Rc<Task>>>>,
    pub queued: Cell<bool>,
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
            task.queue.borrow_mut().push_back(Rc::clone(task));
        }
    }

    #[inline]
    pub fn mark_dequeued(&self) {
        self.queued.set(false);
    }
}
