use std::sync::Arc;

use crossbeam_queue::SegQueue;
use futures_util::future::BoxFuture;
use futures_util::task::ArcWake;
use parking_lot::Mutex;

pub struct Task {
    pub future: Mutex<Option<BoxFuture<'static, ()>>>,
    pub queue: Arc<SegQueue<Arc<Task>>>,
}

impl ArcWake for Task {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        // Re-enqueue the task when it gets woken.
        let cloned = Arc::clone(arc_self);
        arc_self.queue.push(cloned);
    }
}
