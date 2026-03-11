use std::cell::{RefCell, UnsafeCell};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use crossbeam_queue::SegQueue;
use futures_util::FutureExt;
use slab::Slab;

#[cfg(feature = "blocking-default")]
use crate::blocking::DefaultBlockingThreadPool;
use crate::blocking::{BlockingThreadPool, SpawnBlockingError};
use crate::driver::AnyDriver;
use crate::task::Task;
#[cfg(feature = "time")]
use crate::timer::Timer;

thread_local! {
    static CURRENT_RUNTIME: RefCell<Option<Rc<RuntimeInner>>> = const { RefCell::new(None) };
}

pub(crate) struct RuntimeInner {
    queue: Rc<UnsafeCell<VecDeque<Arc<Task>>>>,
    next_task: Rc<RefCell<Option<Arc<Task>>>>,
    remote_queue: Arc<SegQueue<usize>>,
    token_to_task: RefCell<Slab<Arc<Task>>>,
    driver: Rc<AnyDriver>,
    waiting: Arc<AtomicBool>,
    blocking_pool: Option<Box<dyn BlockingThreadPool>>,
    #[cfg(feature = "fs")]
    fs_offload: bool,
    #[cfg(feature = "time")]
    timer: Option<Rc<Timer>>,
}

pub struct Runtime {
    inner: Option<Rc<RuntimeInner>>,
}

/// Internal state for a spawned task.
/// Stores the task's output and a waker to notify when the task completes.
struct JoinState<T> {
    output: Option<T>,
    waker: Option<Waker>,
}

/// A handle to a spawned asynchronous task.
///
/// This handle implements `Future` and can be `await`ed to retrieve the task's output.
/// It allows you to wait for a spawned task to complete and get its result.
pub struct JoinHandle<T> {
    state: Rc<RefCell<JoinState<T>>>,
}

impl<T> JoinHandle<T> {
    /// Creates a new `JoinHandle` with the given state.
    #[inline]
    fn new(state: Rc<RefCell<JoinState<T>>>) -> Self {
        Self { state }
    }

    /// Attempts to retrieve the task's output without blocking.
    /// Returns `Some(output)` if the task has completed, or `None` if it's still running.
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

pub(crate) fn current_driver() -> Option<Rc<AnyDriver>> {
    CURRENT_RUNTIME.with(|runtime| {
        let runtime = runtime.borrow();
        runtime
            .as_ref()
            .map(|runtime_inner| runtime_inner.driver.clone())
    })
}

#[cfg(feature = "time")]
pub(crate) fn current_timer() -> Option<Rc<Timer>> {
    CURRENT_RUNTIME.with(|runtime| {
        let runtime = runtime.borrow();
        runtime.as_ref().map(|runtime_inner| {
            runtime_inner
                .timer
                .as_ref()
                .expect("timer not enabled")
                .clone()
        })
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

pub async fn spawn_blocking<T, F>(f: F) -> Result<T, SpawnBlockingError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let runtime = CURRENT_RUNTIME.with(|runtime| {
        let runtime = runtime.borrow();
        if let Some(runtime_inner) = &*runtime {
            runtime_inner.clone()
        } else {
            panic!("can't spawn a blocking task outside runtime");
        }
    });

    runtime.spawn_blocking(f).await
}

#[cfg(feature = "fs")]
#[inline]
pub(crate) fn offload_fs() -> bool {
    CURRENT_RUNTIME.with(|runtime| {
        let runtime = runtime.borrow();
        if let Some(runtime_inner) = &*runtime {
            runtime_inner.fs_offload
        } else {
            false
        }
    })
}

impl RuntimeInner {
    #[inline]
    pub(crate) fn spawn<T>(&self, future: impl Future<Output = T> + 'static) -> JoinHandle<T>
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

        let mut slab = self.token_to_task.borrow_mut();
        let vacant_slab_entry = slab.vacant_entry();
        let task = Arc::new(Task {
            future: RefCell::new(Some(future)),
            queue: Rc::downgrade(&self.queue),
            next_task: Rc::downgrade(&self.next_task),
            remote_queue: Arc::downgrade(&self.remote_queue),
            queued: AtomicBool::new(true),
            thread_id: std::thread::current().id(),
            interruptor: self.driver.get_interruptor(),
            waiting: Arc::downgrade(&self.waiting),
            token: vacant_slab_entry.key(),
        });
        vacant_slab_entry.insert(task.clone());

        self.enqueue(task);
        JoinHandle::new(state)
    }

    #[inline]
    pub(crate) async fn spawn_blocking<T, F>(&self, f: F) -> Result<T, SpawnBlockingError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let pool = self
            .blocking_pool
            .as_ref()
            .expect("blocking pool not initialized");
        crate::blocking::spawn_blocking(pool.as_ref(), f).await
    }

    #[inline]
    fn enqueue(&self, task: Arc<Task>) {
        // SAFETY: this runtime is single-threaded. All ready-queue mutation goes
        // through runtime/task wake paths on the same thread.
        unsafe {
            (&mut *self.queue.get()).push_back(task);
        }
    }

    #[inline]
    fn drain_ready(&self, batch: &mut Vec<Arc<Task>>) {
        let slab = self.token_to_task.borrow();
        while let Some(token) = self.remote_queue.pop() {
            if let Some(task) = slab.get(token) {
                // SAFETY: this runtime is single-threaded. All ready-queue mutation goes
                // through runtime/task wake paths on the same thread.
                unsafe {
                    (&mut *self.queue.get()).push_front(task.clone());
                }
            }
        }

        // SAFETY: this runtime is single-threaded and we only hold this mutable
        // access while draining the queue before polling any task futures.
        let queue = unsafe { &mut *self.queue.get() };
        let mut budget = 256;
        while let Some(task) = queue.pop_front() {
            task.mark_dequeued();
            batch.push(task);
            budget -= 1;
            if budget == 0 {
                break;
            }
        }
    }

    #[inline]
    fn take_next_task(&self) -> Option<Arc<Task>> {
        let task = self.next_task.take();
        if let Some(task) = &task {
            task.mark_dequeued();
        }
        task
    }
}

impl Runtime {
    /// Create a new runtime with the given driver.
    #[inline]
    pub(crate) fn new(driver: AnyDriver) -> Self {
        #[cfg(not(feature = "blocking-default"))]
        let blocking_pool = None;
        #[cfg(feature = "blocking-default")]
        let blocking_pool: Option<Box<dyn BlockingThreadPool>> =
            Some(Box::new(DefaultBlockingThreadPool::new()));
        Self::with_options(driver, true, blocking_pool, true)
    }

    #[inline]
    pub(crate) fn with_options(
        driver: AnyDriver,
        enable_timer: bool,
        blocking_pool: Option<Box<dyn BlockingThreadPool>>,
        fs_offload: bool,
    ) -> Self {
        #[cfg(not(feature = "fs"))]
        let _ = fs_offload;

        let ready_queue = Rc::new(UnsafeCell::new(VecDeque::with_capacity(4096)));
        Runtime {
            inner: Some(Rc::new(RuntimeInner {
                queue: ready_queue,
                next_task: Rc::new(RefCell::new(None)),
                remote_queue: Arc::new(SegQueue::new()),
                token_to_task: RefCell::new(Slab::with_capacity(4096)),
                driver: Rc::new(driver),
                waiting: Arc::new(AtomicBool::new(false)),
                blocking_pool,
                #[cfg(feature = "fs")]
                fs_offload,
                #[cfg(feature = "time")]
                timer: if enable_timer {
                    Some(Rc::new(Timer::new()))
                } else {
                    None
                },
            })),
        }
    }

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
    pub async fn spawn_blocking<T, F>(&self, f: F) -> Result<T, SpawnBlockingError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let inner = self.inner.as_ref().expect("runtime has been dropped");
        inner.spawn_blocking(f).await
    }

    #[inline]
    pub fn block_on<T>(&self, future: impl Future<Output = T> + 'static) -> T
    where
        T: 'static,
    {
        let inner = self.inner.as_ref().expect("runtime has been dropped");
        let _runtime_guard = CurrentRuntimeGuard::enter(inner.clone());

        let spawned_task = inner.spawn(future);
        let mut batch = Vec::with_capacity(256);

        loop {
            if let Some(output) = spawned_task.try_take_output() {
                return output;
            }

            let mut next_task_taken = false;

            batch.clear();
            if let Some(next_task) = inner.take_next_task() {
                // Fast path: if there's a task ready, run it immediately
                batch.push(next_task);
                next_task_taken = true;
            } else {
                inner.drain_ready(&mut batch);
            }

            if batch.is_empty() {
                inner.waiting.store(true, Ordering::Release);
                #[cfg(feature = "time")]
                let (deadline, woken_up) = if let Some(timer) = inner.timer.as_ref() {
                    // Spin the timing wheel
                    timer.spin_and_get_deadline()
                } else {
                    (None, false)
                };

                #[cfg(feature = "time")]
                if !woken_up {
                    inner.driver.wait(deadline);
                }
                #[cfg(not(feature = "time"))]
                inner.driver.wait(None);

                inner.waiting.store(false, Ordering::Release);
                continue;
            }

            #[cfg(feature = "time")]
            if !next_task_taken && batch.len() > 64 {
                if let Some(timer) = inner.timer.as_ref() {
                    // Spin the timing wheel to avoid starving timers
                    let _ = timer.spin_and_get_deadline();
                }
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
                    } else {
                        // Future completed, remove task from token_to_task slab to prevent memory leaks
                        inner.token_to_task.borrow_mut().remove(task.token);
                    }
                }
            }

            if !next_task_taken {
                inner.driver.flush();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_on_returns_future_output() {
        let runtime = crate::executor::Runtime::new(AnyDriver::new_mock());
        let value = runtime.block_on(async { 42usize });
        assert_eq!(value, 42);
    }

    #[test]
    fn spawn_join_handle_returns_task_output() {
        let runtime = crate::executor::Runtime::new(AnyDriver::new_mock());
        let value = runtime.block_on(async {
            let handle = spawn(async { 21usize });
            handle.await * 2
        });
        assert_eq!(value, 42);
    }

    #[test]
    fn runtime_spawn_returns_join_handle() {
        let runtime = crate::executor::Runtime::new(AnyDriver::new_mock());
        let handle = runtime.spawn(async { 7usize });
        let value = runtime.block_on(async move { handle.await });
        assert_eq!(value, 7);
    }

    #[cfg(feature = "blocking-default")]
    #[test]
    fn spawn_blocking_returns_task_output() {
        let runtime = crate::executor::Runtime::new(AnyDriver::new_mock());
        let value = runtime.block_on(async {
            let handle = spawn_blocking(|| 21usize).await.unwrap();
            handle * 2
        });
        assert_eq!(value, 42);
    }
}
