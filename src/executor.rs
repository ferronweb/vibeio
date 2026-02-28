use std::cell::{RefCell, UnsafeCell};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Waker};

use futures_util::FutureExt;

use crate::driver::AnyDriver;
use crate::task::Task;
use crate::timer::Timer;

thread_local! {
    static CURRENT_RUNTIME: RefCell<Option<Rc<RuntimeInner>>> = RefCell::new(None);
}

pub struct RuntimeInner {
    queue: Rc<UnsafeCell<VecDeque<Rc<Task>>>>,
    driver: Rc<AnyDriver>,
    timer: Rc<Timer>,
    waiting: Rc<AtomicBool>,
}

pub struct Runtime {
    inner: Option<Rc<RuntimeInner>>,
}

struct JoinState<T> {
    output: Option<T>,
    waker: Option<Waker>,
}

pub struct JoinHandle<T> {
    state: Rc<RefCell<JoinState<T>>>,
}

impl<T> JoinHandle<T> {
    #[inline]
    fn new(state: Rc<RefCell<JoinState<T>>>) -> Self {
        Self { state }
    }

    #[inline]
    fn try_take_output(&self) -> Option<T> {
        let mut state = self.state.borrow_mut();
        state.output.take()
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = T;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.state.borrow_mut();
        if let Some(output) = state.output.take() {
            Poll::Ready(output)
        } else {
            state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

struct CurrentRuntimeGuard;

impl CurrentRuntimeGuard {
    #[inline]
    fn enter(runtime_inner: Rc<RuntimeInner>) -> Self {
        CURRENT_RUNTIME.with(|runtime| {
            let mut runtime = runtime.borrow_mut();
            if runtime.is_some() {
                panic!("can't spawn a runtime inside another runtime");
            }

            *runtime = Some(runtime_inner);
        });

        Self
    }
}

impl Drop for CurrentRuntimeGuard {
    #[inline]
    fn drop(&mut self) {
        CURRENT_RUNTIME.with(|runtime| {
            let mut runtime = runtime.borrow_mut();
            *runtime = None;
        });
    }
}

pub fn new_runtime(driver: AnyDriver) -> Runtime {
    let ready_queue = Rc::new(UnsafeCell::new(VecDeque::with_capacity(64)));
    Runtime {
        inner: Some(Rc::new(RuntimeInner {
            queue: ready_queue,
            driver: Rc::new(driver),
            waiting: Rc::new(AtomicBool::new(false)),
            timer: Rc::new(Timer::new()),
        })),
    }
}

pub(crate) fn current_driver() -> Option<Rc<AnyDriver>> {
    CURRENT_RUNTIME.with(|runtime| {
        let runtime = runtime.borrow();
        runtime
            .as_ref()
            .map(|runtime_inner| runtime_inner.driver.clone())
    })
}

pub fn spawn<T>(future: impl Future<Output = T> + 'static) -> JoinHandle<T>
where
    T: 'static,
{
    let runtime = CURRENT_RUNTIME.with(|runtime| {
        let runtime = runtime.borrow();
        if let Some(runtime_inner) = &*runtime {
            runtime_inner.clone()
        } else {
            panic!("can't spawn a task outside runtime");
        }
    });

    runtime.spawn(future)
}

impl RuntimeInner {
    #[inline]
    pub fn spawn<T>(&self, future: impl Future<Output = T> + 'static) -> JoinHandle<T>
    where
        T: 'static,
    {
        let state = Rc::new(RefCell::new(JoinState {
            output: None,
            waker: None,
        }));
        let state_for_task = state.clone();
        let future = async move {
            let output = future.await;
            let mut state = state_for_task.borrow_mut();
            state.output = Some(output);
            if let Some(waker) = state.waker.take() {
                waker.wake();
            }
        }
        .boxed_local();

        let task = Rc::new(Task {
            future: RefCell::new(Some(future)),
            queue: Rc::downgrade(&self.queue),
            queued: std::cell::Cell::new(true),
            thread_id: std::thread::current().id(),
            driver: Rc::downgrade(&self.driver),
            waiting: Rc::downgrade(&self.waiting),
        });

        self.enqueue(task);
        JoinHandle::new(state)
    }

    #[inline]
    fn enqueue(&self, task: Rc<Task>) {
        // SAFETY: this runtime is single-threaded. All ready-queue mutation goes
        // through runtime/task wake paths on the same thread.
        unsafe {
            (&mut *self.queue.get()).push_back(task);
        }
    }

    #[inline]
    fn drain_ready(&self, batch: &mut Vec<Rc<Task>>) {
        // SAFETY: this runtime is single-threaded and we only hold this mutable
        // access while draining the queue before polling any task futures.
        let queue = unsafe { &mut *self.queue.get() };
        while let Some(task) = queue.pop_front() {
            task.mark_dequeued();
            batch.push(task);
        }
    }
}

impl Runtime {
    #[inline]
    pub fn spawn<T>(&self, future: impl Future<Output = T> + 'static) -> JoinHandle<T>
    where
        T: 'static,
    {
        self.inner
            .as_ref()
            .expect("runtime has been dropped")
            .spawn(future)
    }

    #[inline]
    pub fn block_on<T>(&self, future: impl Future<Output = T> + 'static) -> T
    where
        T: 'static,
    {
        let inner = self.inner.as_ref().expect("runtime has been dropped");
        let _runtime_guard = CurrentRuntimeGuard::enter(inner.clone());

        let spawned_task = inner.spawn(future);
        let mut batch = Vec::with_capacity(64);

        loop {
            if let Some(output) = spawned_task.try_take_output() {
                return output;
            }

            batch.clear();
            inner.drain_ready(&mut batch);

            if batch.is_empty() {
                // Spin timing wheel
                let deadline = inner.timer.spin_and_get_deadline();

                // Wait for I/O
                inner.waiting.store(true, Ordering::Relaxed);
                inner.driver.wait(deadline);
                inner.waiting.store(false, Ordering::Relaxed);
                continue;
            }

            for task in batch.drain(..) {
                let mut future_slot = task.future.borrow_mut();
                if let Some(mut future) = future_slot.take() {
                    drop(future_slot);
                    let waker = task.waker();
                    let mut context = Context::from_waker(&waker);

                    if future.as_mut().poll(&mut context).is_pending() {
                        let mut future_slot = task.future.borrow_mut();
                        *future_slot = Some(future);
                    }
                }
            }
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // Drop all tasks with current runtime entered
        let inner = self.inner.take().expect("runtime has been dropped");
        let _runtime_guard = CurrentRuntimeGuard::enter(inner.clone());
        drop(inner);
        drop(_runtime_guard);
    }
}

pub struct YieldNow(bool);

pub fn yield_now() -> YieldNow {
    YieldNow(false)
}

impl Future for YieldNow {
    type Output = ();

    #[inline]
    fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.0 {
            Poll::Ready(())
        } else {
            self.0 = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_on_returns_future_output() {
        let runtime = new_runtime(AnyDriver::new_mock());
        let value = runtime.block_on(async { 42usize });
        assert_eq!(value, 42);
    }

    #[test]
    fn spawn_join_handle_returns_task_output() {
        let runtime = new_runtime(AnyDriver::new_mock());
        let value = runtime.block_on(async {
            let handle = spawn(async { 21usize });
            handle.await * 2
        });
        assert_eq!(value, 42);
    }

    #[test]
    fn runtime_spawn_returns_join_handle() {
        let runtime = new_runtime(AnyDriver::new_mock());
        let handle = runtime.spawn(async { 7usize });
        let value = runtime.block_on(async move { handle.await });
        assert_eq!(value, 7);
    }
}
