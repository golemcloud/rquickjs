use crate::{
    class::{self, ffi::VTable, JsClass},
    qjs, Ctx, Error, JsLifetime, Object, Value,
};

use super::{
    exotic::ExoticMethodsHolder,
    userdata::{UserDataGuard, UserDataMap},
    InterruptHandler, JobOwnerRedirect, PromiseHook, PromiseHookType, RejectionTracker,
    UserDataError,
};
use alloc::{
    boxed::Box,
    collections::{BTreeMap, VecDeque},
};
use core::{
    any::{Any, TypeId},
    cell::{Cell, RefCell, UnsafeCell},
    ffi::c_void,
    marker::PhantomData,
    ptr::{self, NonNull},
};

#[cfg(feature = "std")]
use std::collections::{hash_map::Entry, HashMap};

#[cfg(not(feature = "std"))]
use hashbrown::{hash_map::Entry, HashMap};

#[cfg(feature = "futures")]
use super::{schedular::SchedularPoll, spawner::Spawner};

#[cfg(feature = "futures")]
use core::{
    future::Future,
    task::{Context, Waker},
};

/// Opaque book keeping data for Rust.
pub(crate) struct Opaque<'js> {
    /// Used to carry a panic if a callback triggered one.
    panic: Cell<Option<Box<dyn Any + Send + 'static>>>,

    /// The user provided promise hook, if any.
    promise_hook: UnsafeCell<Option<PromiseHook>>,

    /// The user provided rejection tracker, if any.
    rejection_tracker: UnsafeCell<Option<RejectionTracker>>,

    /// The user provided interrupt handler, if any.
    interrupt_handler: UnsafeCell<Option<InterruptHandler>>,

    /// The class id for rust classes.
    class_id: qjs::JSClassID,
    /// The class id for rust classes which can be called.
    callable_class_id: qjs::JSClassID,
    /// The class id for exotic classes
    exotic_class_id: qjs::JSClassID,

    prototypes: UnsafeCell<HashMap<TypeId, Option<Object<'js>>>>,

    userdata: UserDataMap,

    #[cfg(feature = "futures")]
    spawner: Option<UnsafeCell<Spawner>>,

    /// The jobs QuickJS handed over through its enqueue hook, per job owner, in enqueue order.
    jobs: RefCell<BTreeMap<u64, VecDeque<NonNull<qjs::JSJob>>>>,

    /// The job owner whose jobs and spawned futures run now.
    job_owner: Cell<u64>,

    /// Chooses the owner of a job enqueued for an owner other than the current one.
    job_owner_redirect: Cell<Option<JobOwnerRedirect>>,

    exotic_methods: ExoticMethodsHolder,

    _marker: PhantomData<&'js ()>,
}

impl<'js> Opaque<'js> {
    pub fn new() -> Self {
        Opaque {
            panic: Cell::new(None),

            promise_hook: UnsafeCell::new(None),

            rejection_tracker: UnsafeCell::new(None),

            interrupt_handler: UnsafeCell::new(None),

            class_id: qjs::JS_INVALID_CLASS_ID,
            callable_class_id: qjs::JS_INVALID_CLASS_ID,
            exotic_class_id: qjs::JS_INVALID_CLASS_ID,

            prototypes: UnsafeCell::new(HashMap::new()),

            userdata: UserDataMap::default(),

            _marker: PhantomData,

            #[cfg(feature = "futures")]
            spawner: None,

            jobs: RefCell::new(BTreeMap::new()),
            job_owner: Cell::new(0),
            job_owner_redirect: Cell::new(None),

            exotic_methods: ExoticMethodsHolder::new(),
        }
    }

    #[cfg(feature = "futures")]
    pub fn with_spawner() -> Self {
        let mut this = Opaque::new();
        this.spawner = Some(UnsafeCell::new(Spawner::new()));
        this
    }

    pub unsafe fn initialize(&mut self, rt: *mut qjs::JSRuntime) -> Result<(), Error> {
        qjs::JS_SetJobHooks(rt, Some(job_tag), Some(enqueue_job), ptr::null_mut());
        qjs::JS_NewClassID(rt, (&mut self.class_id) as *mut qjs::JSClassID);
        qjs::JS_NewClassID(rt, (&mut self.callable_class_id) as *mut qjs::JSClassID);
        qjs::JS_NewClassID(rt, (&mut self.exotic_class_id) as *mut qjs::JSClassID);

        let class_def = qjs::JSClassDef {
            class_name: c"RustClass".as_ptr().cast(),
            finalizer: Some(class::ffi::class_finalizer),
            gc_mark: Some(class::ffi::class_trace),
            call: None,
            exotic: ptr::null_mut(),
        };

        if 0 != qjs::JS_NewClass(rt, self.class_id, &class_def) {
            return Err(Error::Unknown);
        }

        let class_def = qjs::JSClassDef {
            class_name: c"RustFunction".as_ptr().cast(),
            finalizer: Some(class::ffi::callable_finalizer),
            gc_mark: Some(class::ffi::callable_trace),
            call: Some(class::ffi::call),
            exotic: ptr::null_mut(),
        };

        if 0 != qjs::JS_NewClass(rt, self.callable_class_id, &class_def) {
            return Err(Error::Unknown);
        }

        let class_def = qjs::JSClassDef {
            class_name: c"RustExotic".as_ptr().cast(),
            finalizer: Some(class::ffi::exotic_class_finalizer),
            gc_mark: Some(class::ffi::exotic_class_trace),
            call: None,
            exotic: self.exotic_methods.as_ptr(),
        };

        if 0 != qjs::JS_NewClass(rt, self.exotic_class_id, &class_def) {
            return Err(Error::Unknown);
        }

        Ok(())
    }

    pub unsafe fn from_runtime_ptr<'a>(rt: *mut qjs::JSRuntime) -> &'a Self {
        &*(qjs::JS_GetRuntimeOpaque(rt).cast::<Self>())
    }

    #[cfg(feature = "futures")]
    fn spawner(&self) -> &UnsafeCell<Spawner> {
        self.spawner
            .as_ref()
            .expect("tried to use async function in non async runtime")
    }

    /// The job owner whose jobs and spawned futures run now.
    pub fn job_owner(&self) -> u64 {
        self.job_owner.get()
    }

    /// Makes `owner` the current job owner and returns the previous one.
    pub fn set_job_owner(&self, owner: u64) -> u64 {
        self.job_owner.replace(owner)
    }

    pub fn set_job_owner_redirect(&self, redirect: Option<JobOwnerRedirect>) {
        self.job_owner_redirect.set(redirect)
    }

    /// Returns whether the current job owner has a job.
    pub fn is_job_pending(&self) -> bool {
        self.jobs.borrow().contains_key(&self.job_owner())
    }

    /// Removes the first job of the current job owner. The caller owns the job and must pass it
    /// to `JS_ExecuteJob` or `JS_FreeJob`.
    pub fn take_job(&self) -> Option<NonNull<qjs::JSJob>> {
        Self::pop_job(&mut self.jobs.borrow_mut(), self.job_owner())
    }

    fn pop_job(
        jobs: &mut BTreeMap<u64, VecDeque<NonNull<qjs::JSJob>>>,
        owner: u64,
    ) -> Option<NonNull<qjs::JSJob>> {
        let queue = jobs.get_mut(&owner)?;
        let job = queue.pop_front();
        if queue.is_empty() {
            jobs.remove(&owner);
        }
        job
    }

    /// Frees every job of every owner, including jobs that freeing enqueues.
    ///
    /// Must be called before the runtime is freed, which requires that no job is left.
    pub unsafe fn free_jobs(&self, rt: *mut qjs::JSRuntime) {
        loop {
            let job = {
                let mut jobs = self.jobs.borrow_mut();
                let Some(owner) = jobs.keys().next().copied() else {
                    return;
                };
                Self::pop_job(&mut jobs, owner)
            };
            if let Some(job) = job {
                qjs::JS_FreeJob(rt, job.as_ptr());
            }
        }
    }

    /// Spawns `f` under the current job owner.
    #[cfg(feature = "futures")]
    pub unsafe fn push<F>(&self, f: F)
    where
        F: Future<Output = ()>,
    {
        (*self.spawner().get()).push(self.job_owner(), f)
    }

    /// Registers a waker that is woken when a future is spawned.
    #[cfg(feature = "futures")]
    pub fn listen(&self, wake: Waker) {
        unsafe { (*self.spawner().get()).listen(wake) };
    }

    /// Returns whether no future is spawned under the current job owner.
    #[cfg(feature = "futures")]
    pub fn spawner_is_empty(&self) -> bool {
        unsafe { (*self.spawner().get()).is_empty(self.job_owner()) }
    }

    /// Polls the futures spawned under the current job owner.
    #[cfg(feature = "futures")]
    pub fn poll(&self, cx: &mut Context) -> SchedularPoll {
        unsafe { (*self.spawner().get()).poll(self.job_owner(), cx) }
    }

    pub fn insert_userdata<U>(&self, data: U) -> Result<Option<Box<U>>, UserDataError<U>>
    where
        U: JsLifetime<'js>,
        U::Changed<'static>: Any,
    {
        self.userdata.insert(data)
    }

    pub fn remove_userdata<U>(&self) -> Result<Option<Box<U>>, UserDataError<()>>
    where
        U: JsLifetime<'js>,
        U::Changed<'static>: Any,
    {
        self.userdata.remove()
    }

    pub fn get_userdata<U>(&self) -> Option<UserDataGuard<U>>
    where
        U: JsLifetime<'js>,
        U::Changed<'static>: Any,
    {
        self.userdata.get()
    }

    pub fn set_promise_hook(&self, promise_hook: Option<PromiseHook>) {
        unsafe { (*self.promise_hook.get()) = promise_hook }
    }

    pub fn run_promise_hook<'a>(
        &self,
        ctx: Ctx<'a>,
        type_: PromiseHookType,
        promise: Value<'a>,
        parent: Value<'a>,
    ) {
        unsafe { (*self.promise_hook.get()).as_mut().unwrap()(ctx, type_, promise, parent) }
    }

    pub fn set_rejection_tracker(&self, tracker: Option<RejectionTracker>) {
        unsafe { (*self.rejection_tracker.get()) = tracker }
    }

    pub fn run_rejection_tracker<'a>(
        &self,
        ctx: Ctx<'a>,
        promise: Value<'a>,
        reason: Value<'a>,
        is_handled: bool,
    ) {
        unsafe {
            (*self.rejection_tracker.get()).as_mut().unwrap()(ctx, promise, reason, is_handled)
        }
    }

    pub fn set_interrupt_handler(&self, interupt: Option<InterruptHandler>) {
        unsafe { (*self.interrupt_handler.get()) = interupt }
    }

    pub fn run_interrupt_handler(&self) -> bool {
        unsafe { (*self.interrupt_handler.get()).as_mut().unwrap()() }
    }

    #[allow(dead_code)] // not used in no_std
    pub fn set_panic(&self, panic: Box<dyn Any + Send + 'static>) {
        self.panic.set(Some(panic))
    }

    pub fn take_panic(&self) -> Option<Box<dyn Any + Send + 'static>> {
        self.panic.take()
    }

    pub fn get_class_id(&self) -> qjs::JSClassID {
        self.class_id
    }

    pub fn get_callable_id(&self) -> qjs::JSClassID {
        self.callable_class_id
    }

    pub fn get_exotic_id(&self) -> qjs::JSClassID {
        self.exotic_class_id
    }

    pub fn get_or_insert_prototype<C: JsClass<'js>>(
        &self,
        ctx: &Ctx<'js>,
    ) -> Result<Option<Object<'js>>, Error> {
        unsafe {
            let vtable = VTable::get::<C>();
            let id = vtable.id();
            match (*self.prototypes.get()).entry(id) {
                Entry::Occupied(x) => Ok(x.get().clone()),
                Entry::Vacant(x) => {
                    let proto = C::prototype(ctx)?;
                    Ok(x.insert(proto).clone())
                }
            }
        }
    }

    /// Cleans up all the internal state.
    ///
    /// Called before dropping the runtime to ensure that we drop everything before freeing the
    /// runtime.
    pub fn clear(&mut self) {
        self.rejection_tracker.get_mut().take();
        self.interrupt_handler.get_mut().take();
        self.panic.take();
        self.prototypes.get_mut().clear();
        #[cfg(feature = "futures")]
        self.spawner.take();
        self.userdata.clear();
    }
}

unsafe extern "C" fn job_tag(rt: *mut qjs::JSRuntime, _opaque: *mut c_void) -> u64 {
    unsafe { Opaque::from_runtime_ptr(rt) }.job_owner()
}

/// Queues a job for its tag's owner, or for the owner the redirect chooses when that owner is not
/// current. QuickJS calls this while it settles a promise or collects garbage, so it must not call
/// into QuickJS.
unsafe extern "C" fn enqueue_job(
    rt: *mut qjs::JSRuntime,
    job: *mut qjs::JSJob,
    tag: u64,
    _opaque: *mut c_void,
) {
    let opaque = unsafe { Opaque::from_runtime_ptr(rt) };
    let current = opaque.job_owner();
    let owner = match opaque.job_owner_redirect.get() {
        Some(redirect) if tag != current => redirect(tag, current),
        _ => tag,
    };
    let job = NonNull::new(job).expect("QuickJS enqueued a null job");
    opaque
        .jobs
        .borrow_mut()
        .entry(owner)
        .or_default()
        .push_back(job);
}
