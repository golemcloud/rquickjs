use super::{
    schedular::{Schedular, SchedularPoll},
    AsyncWeakRuntime, InnerRuntime,
};
use crate::AsyncRuntime;
use alloc::{collections::BTreeMap, rc::Rc, vec::Vec};
use core::{
    cell::RefCell,
    future::Future,
    pin::Pin,
    task::{ready, Context, Poll, Waker},
};

use async_lock::futures::LockArc;

/// A structure to hold futures spawned inside the runtime.
///
/// Futures are kept per job owner (see `runtime::job_owner`): a future spawned while an owner is
/// current is only polled while that owner is current, so it is only driven by that owner's
/// drivers. An owner's entry is dropped once nothing is spawned under it.
pub struct Spawner {
    schedulars: RefCell<BTreeMap<u64, Rc<Schedular>>>,
    wakeup: RefCell<Vec<Waker>>,
}

impl Spawner {
    pub fn new() -> Self {
        Spawner {
            schedulars: RefCell::new(BTreeMap::new()),
            wakeup: RefCell::new(Vec::new()),
        }
    }

    pub unsafe fn push<F>(&self, owner: u64, f: F)
    where
        F: Future<Output = ()>,
    {
        let schedular = self
            .schedulars
            .borrow_mut()
            .entry(owner)
            .or_insert_with(|| Rc::new(Schedular::new()))
            .clone();
        unsafe { schedular.push(f) };
        let wakeup = core::mem::take(&mut *self.wakeup.borrow_mut());
        wakeup.into_iter().for_each(Waker::wake);
    }

    /// Registers a waker that is woken when a future is spawned under any owner.
    pub fn listen(&self, wake: Waker) {
        let mut wakeup = self.wakeup.borrow_mut();
        if !wakeup.iter().any(|existing| existing.will_wake(&wake)) {
            wakeup.push(wake);
        }
    }

    pub fn is_empty(&self, owner: u64) -> bool {
        self.schedulars
            .borrow()
            .get(&owner)
            .is_none_or(|schedular| schedular.is_empty())
    }

    pub fn poll(&self, owner: u64, cx: &mut Context) -> SchedularPoll {
        let Some(schedular) = self.schedulars.borrow().get(&owner).cloned() else {
            return SchedularPoll::Empty;
        };
        let result = unsafe { schedular.poll(cx) };
        let idle = schedular.is_empty();
        drop(schedular);
        if idle {
            let mut schedulars = self.schedulars.borrow_mut();
            if schedulars
                .get(&owner)
                .is_some_and(|schedular| Rc::strong_count(schedular) == 1 && schedular.is_empty())
            {
                schedulars.remove(&owner);
            }
        }
        result
    }
}

enum DriveFutureState {
    Initial,
    Lock {
        lock_future: Option<LockArc<InnerRuntime>>,
        // Here to ensure the lock remains valid.
        _runtime: AsyncRuntime,
    },
}

pub struct DriveFuture {
    rt: AsyncWeakRuntime,
    state: DriveFutureState,
}

#[cfg(feature = "parallel")]
unsafe impl Send for DriveFuture {}
#[cfg(feature = "parallel")]
unsafe impl Sync for DriveFuture {}

impl DriveFuture {
    pub(crate) fn new(rt: AsyncWeakRuntime) -> Self {
        Self {
            rt,
            state: DriveFutureState::Initial,
        }
    }
}

impl Future for DriveFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut core::task::Context<'_>) -> Poll<Self::Output> {
        // Safety: We manually ensure that pinned values remained properly pinned.
        let this = unsafe { self.get_unchecked_mut() };
        loop {
            let mut lock = match this.state {
                DriveFutureState::Initial => {
                    let Some(_runtime) = this.rt.try_ref() else {
                        return Poll::Ready(());
                    };

                    let lock_future = _runtime.inner.lock_arc();
                    this.state = DriveFutureState::Lock {
                        lock_future: Some(lock_future),
                        _runtime,
                    };
                    continue;
                }
                DriveFutureState::Lock {
                    ref mut lock_future,
                    ..
                } => {
                    // Safety: The future will not be moved until it is ready and then dropped.
                    let res = unsafe {
                        ready!(Pin::new_unchecked(lock_future.as_mut().unwrap()).poll(cx))
                    };
                    // Assign none explicitly so it we don't move out of the future.
                    *lock_future = None;
                    res
                }
            };

            lock.runtime.update_stack_top();

            lock.runtime.get_opaque().listen(cx.waker().clone());

            loop {
                // TODO: Handle error.
                if let Ok(true) = lock.runtime.execute_pending_job() {
                    continue;
                }

                // TODO: Handle error.
                match lock.runtime.get_opaque().poll(cx) {
                    SchedularPoll::ShouldYield | SchedularPoll::Empty | SchedularPoll::Pending => {
                        break
                    }
                    SchedularPoll::PendingProgress => {}
                }
            }

            this.state = DriveFutureState::Initial;
            return Poll::Pending;
        }
    }
}
