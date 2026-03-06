use std::{thread, time::Duration};

use crossbeam_channel::{Receiver, Sender};
use crossbeam_queue::ArrayQueue;

use super::BlockingThreadPool;

/// A task that can be executed by the thread pool.
type Task = Box<dyn FnOnce() + Send + 'static>;

/// A default implementation of `BlockingThreadPool` using `std::thread` and a work-stealing queue.
pub struct DefaultBlockingThreadPool {
    task_queue: Option<(Sender<Task>, Receiver<Task>)>,
    threads: ArrayQueue<thread::JoinHandle<()>>,
    wait_on_exit: bool,
}

impl DefaultBlockingThreadPool {
    /// Creates a new `DefaultBlockingThreadPool` with the default maximum number of threads.
    #[inline]
    pub fn new() -> Self {
        Self::with_max_threads(512)
    }

    /// Creates a new `DefaultBlockingThreadPool` with the specified maximum number of threads.
    #[inline]
    pub fn with_max_threads(num_threads: usize) -> Self {
        Self::with_max_threads_and_wait_on_exit(num_threads, false)
    }

    /// Creates a new `DefaultBlockingThreadPool` with the specified maximum number of threads,
    /// and whether to wait for blocking tasks to complete on exit.
    #[inline]
    pub fn with_max_threads_and_wait_on_exit(num_threads: usize, wait_on_exit: bool) -> Self {
        Self {
            task_queue: Some(crossbeam_channel::unbounded()),
            threads: ArrayQueue::new(num_threads),
            wait_on_exit,
        }
    }
}

impl BlockingThreadPool for DefaultBlockingThreadPool {
    #[inline]
    fn spawn(&self, task: Box<dyn FnOnce() + Send + 'static>) {
        if let Some((tx, rx)) = self.task_queue.as_ref() {
            let _ = tx.try_send(task);
            if !tx.is_empty() && !self.threads.is_full() {
                let rx = rx.clone();
                self.threads.force_push(thread::spawn(move || {
                    while let Ok(task) = rx.recv_timeout(Duration::from_secs(1)) {
                        task();
                    }
                }));
            }
        }
    }
}

impl Drop for DefaultBlockingThreadPool {
    #[inline]
    fn drop(&mut self) {
        let _ = self.task_queue.take();
        if self.wait_on_exit {
            while let Some(thread) = self.threads.pop() {
                let _ = thread.join();
            }
        }
    }
}
