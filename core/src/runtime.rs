//! QuickJS runtime related types.

mod base;
mod exotic;
pub(crate) mod opaque;
pub(crate) mod raw;
mod userdata;

#[cfg(feature = "futures")]
mod r#async;
#[cfg(feature = "futures")]
pub(crate) mod schedular;
#[cfg(feature = "futures")]
mod spawner;
#[cfg(feature = "futures")]
pub use spawner::DriveFuture;

use alloc::boxed::Box;
pub use base::{Runtime, WeakRuntime};
pub use userdata::{UserDataError, UserDataGuard};

#[cfg(feature = "futures")]
pub(crate) use r#async::InnerRuntime;
#[cfg(feature = "futures")]
pub use r#async::{AsyncRuntime, AsyncWeakRuntime};

use crate::value::promise::PromiseHookType;
use crate::{Ctx, Value};

/// The type of the promise hook.
#[cfg(not(feature = "parallel"))]
pub type PromiseHook =
    Box<dyn for<'a> Fn(Ctx<'a>, PromiseHookType, Value<'a>, Value<'a>) + 'static>;
/// The type of the promise hook.
#[cfg(feature = "parallel")]
pub type PromiseHook =
    Box<dyn for<'a> Fn(Ctx<'a>, PromiseHookType, Value<'a>, Value<'a>) + Send + 'static>;

/// The type of the promise rejection tracker.
#[cfg(not(feature = "parallel"))]
pub type RejectionTracker = Box<dyn for<'a> Fn(Ctx<'a>, Value<'a>, Value<'a>, bool) + 'static>;
/// The type of the promise rejection tracker.
#[cfg(feature = "parallel")]
pub type RejectionTracker =
    Box<dyn for<'a> Fn(Ctx<'a>, Value<'a>, Value<'a>, bool) + Send + 'static>;

/// The type of the interrupt handler.
#[cfg(not(feature = "parallel"))]
pub type InterruptHandler = Box<dyn FnMut() -> bool + 'static>;
/// The type of the interrupt handler.
#[cfg(feature = "parallel")]
pub type InterruptHandler = Box<dyn FnMut() -> bool + Send + 'static>;

/// Chooses the owner of a job whose tag names an owner other than the current one, such as a
/// promise reaction that `registered` registered and that is triggered while `current` is
/// current. It must not call into QuickJS or panic.
pub type JobOwnerRedirect = fn(registered: u64, current: u64) -> u64;

/// Job owners partition the jobs and spawned futures of a runtime between embedder-defined
/// owners, such as the concurrent calls of a component.
///
/// Every job belongs to the owner that was current when it was created: a promise reaction job to
/// the owner that registered the reaction (`then`, `await`), and any other job to the owner that
/// enqueued it. A future spawned with `Ctx::spawn` belongs to the owner that spawned it. Executing
/// pending jobs and polling spawned futures only reaches the current owner's work. The default
/// owner is 0, so a runtime that never sets an owner behaves as one queue.
pub mod job_owner {
    use super::{opaque::Opaque, JobOwnerRedirect};
    use crate::qjs;

    /// Returns the current job owner of `rt`.
    ///
    /// # Safety
    /// `rt` must be a live runtime created by this crate.
    pub unsafe fn get(rt: *mut qjs::JSRuntime) -> u64 {
        unsafe { Opaque::from_runtime_ptr(rt) }.job_owner()
    }

    /// Makes `owner` the current job owner of `rt` and returns the previous one.
    ///
    /// # Safety
    /// `rt` must be a live runtime created by this crate.
    pub unsafe fn set(rt: *mut qjs::JSRuntime, owner: u64) -> u64 {
        unsafe { Opaque::from_runtime_ptr(rt) }.set_job_owner(owner)
    }

    /// Sets the function that chooses the owner of a job enqueued for an owner that is not
    /// current. Without one, such a job keeps its owner.
    ///
    /// # Safety
    /// `rt` must be a live runtime created by this crate.
    pub unsafe fn set_redirect(rt: *mut qjs::JSRuntime, redirect: Option<JobOwnerRedirect>) {
        unsafe { Opaque::from_runtime_ptr(rt) }.set_job_owner_redirect(redirect)
    }
}

/// A struct with information about the runtimes memory usage.
pub type MemoryUsage = crate::qjs::JSMemoryUsage;
