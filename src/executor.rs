use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use crossbeam_queue::SegQueue;
use futures_util::FutureExt;
use parking_lot::Mutex;

use crate::driver::AnyDriver;
use crate::task::Task;

thread_local! {
    static CURRENT_RUNTIME: Mutex<Option<Rc<RuntimeInner>>> = Mutex::new(None);
}

pub struct RuntimeInner {
    queue: Rc<SegQueue<Rc<Task>>>,
    driver: Rc<AnyDriver>,
}

pub struct Runtime {
    inner: Rc<RuntimeInner>,
}

struct JoinState<T> {
    output: Option<T>,
    waker: Option<Waker>,
}

pub struct JoinHandle<T> {
    state: Rc<Mutex<JoinState<T>>>,
}

impl<T> JoinHandle<T> {
    #[inline]
    fn new(state: Rc<Mutex<JoinState<T>>>) -> Self {
        Self { state }
    }

    #[inline]
    fn try_take_output(&self) -> Option<T> {
        let mut state = self.state.lock();
        state.output.take()
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = T;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.state.lock();
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
            let mut runtime = runtime.lock();
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
            let mut runtime = runtime.lock();
            *runtime = None;
        });
    }
}

pub fn new_runtime(driver: AnyDriver) -> Runtime {
    let ready_queue = Rc::new(SegQueue::new());
    Runtime {
        inner: Rc::new(RuntimeInner {
            queue: ready_queue,
            driver: Rc::new(driver),
        }),
    }
}

pub(crate) fn current_driver() -> Option<Rc<AnyDriver>> {
    CURRENT_RUNTIME.with(|runtime| {
        let runtime = runtime.lock();
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
        let runtime = runtime.lock();
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
        let state = Rc::new(Mutex::new(JoinState {
            output: None,
            waker: None,
        }));
        let state_for_task = state.clone();
        let future = async move {
            let output = future.await;
            let mut state = state_for_task.lock();
            state.output = Some(output);
            if let Some(waker) = state.waker.take() {
                waker.wake();
            }
        }
        .boxed_local();

        let task = Rc::new(Task {
            future: Mutex::new(Some(future)),
            queue: self.queue.clone(),
        });

        self.queue.push(task);
        JoinHandle::new(state)
    }
}

impl Runtime {
    #[inline]
    pub fn spawn<T>(&self, future: impl Future<Output = T> + 'static) -> JoinHandle<T>
    where
        T: 'static,
    {
        self.inner.spawn(future)
    }

    #[inline]
    pub fn block_on<T>(&self, future: impl Future<Output = T> + 'static) -> T
    where
        T: 'static,
    {
        let _runtime_guard = CurrentRuntimeGuard::enter(self.inner.clone());

        let spawned_task = self.inner.spawn(future);

        loop {
            if let Some(output) = spawned_task.try_take_output() {
                return output;
            }

            if let Some(task) = self.inner.queue.pop() {
                let mut future_slot = task.future.lock();

                if let Some(mut future) = future_slot.take() {
                    let waker = task.waker();
                    let mut context = Context::from_waker(&waker);

                    if future.as_mut().poll(&mut context).is_pending() {
                        *future_slot = Some(future);
                    }
                }
            } else {
                // Wait for I/O
                self.inner.driver.wait();
            }
        }
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
