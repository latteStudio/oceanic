use alloc::{boxed::Box, string::String, sync::Arc};
use core::{
    fmt,
    ops::{Deref, DerefMut},
    time::Duration,
};

use derive_builder::Builder;
use paging::LAddr;
use spin::Mutex;

use super::{ctx, hdl::HandleMap, idle, sig::Signal, tid, Tid, Type};
use crate::{
    cpu::{time::Instant, CpuMask},
    mem::space::Space,
    sched::{ipc::Channel, wait::WaitCell, PREEMPT},
};

#[derive(Debug, Builder)]
#[builder(no_std)]
pub struct TaskInfo {
    from: Option<Tid>,
    #[builder(setter(skip))]
    ret_cell: Arc<WaitCell<usize>>,
    #[builder(setter(skip))]
    excep_chan: Arc<Mutex<Option<Channel>>>,

    name: String,
    ty: Type,

    affinity: CpuMask,

    #[builder(setter(skip))]
    handles: HandleMap,
    #[builder(setter(skip))]
    signal: Mutex<Option<Signal>>,
}

impl TaskInfo {
    #[inline]
    pub fn builder() -> TaskInfoBuilder {
        TaskInfoBuilder::default()
    }

    #[inline]
    pub fn from(&self) -> Option<Tid> {
        self.from.clone()
    }

    #[inline]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[inline]
    pub fn ty(&self) -> Type {
        self.ty
    }

    #[inline]
    pub fn affinity(&self) -> crate::cpu::CpuMask {
        self.affinity
    }

    #[inline]
    pub fn handles(&self) -> &HandleMap {
        &self.handles
    }

    #[inline]
    pub fn ret_cell(&self) -> &Arc<WaitCell<usize>> {
        &self.ret_cell
    }

    #[inline]
    pub fn with_signal<F, R>(&self, func: F) -> R
    where
        F: FnOnce(&mut Option<Signal>) -> R,
    {
        PREEMPT.scope(|| func(&mut self.signal.lock()))
    }

    #[inline]
    pub fn excep_chan(&self) -> Arc<Mutex<Option<Channel>>> {
        Arc::clone(&self.excep_chan)
    }

    #[inline]
    pub fn with_excep_chan<F, R>(&self, func: F) -> R
    where
        F: FnOnce(&mut Option<Channel>) -> R,
    {
        PREEMPT.scope(|| func(&mut self.excep_chan.lock()))
    }
}

#[derive(Debug)]
pub struct Context {
    pub(in crate::sched) tid: Tid,

    pub(in crate::sched) space: Arc<Space>,
    pub(in crate::sched) kstack: ctx::Kstack,
    pub(in crate::sched) ext_frame: ctx::ExtFrame,

    pub(in crate::sched) cpu: usize,
    pub(in crate::sched) runtime: Duration,
}

impl Context {
    #[inline]
    pub fn tid(&self) -> &Tid {
        &self.tid
    }

    #[inline]
    pub fn kstack_mut(&mut self) -> &mut ctx::Kstack {
        &mut self.kstack
    }
}

#[derive(Clone, Copy)]
pub union RunningState {
    start_time: Instant,
    value: i128,
}

impl RunningState {
    pub const NOT_RUNNING_VALUE: i128 = 0;
    pub const NEED_RESCHED_VALUE: i128 = -1;
    pub const NOT_RUNNING: RunningState = RunningState {
        value: Self::NOT_RUNNING_VALUE,
    };
    pub const NEED_RESCHED: RunningState = RunningState {
        value: Self::NEED_RESCHED_VALUE,
    };

    #[inline]
    pub const fn running(start_time: Instant) -> RunningState {
        RunningState { start_time }
    }

    #[inline]
    pub const fn value(&self) -> i128 {
        unsafe { self.value }
    }

    #[inline]
    pub const fn start_time(self) -> Option<Instant> {
        match unsafe { self.value } {
            Self::NOT_RUNNING_VALUE | Self::NEED_RESCHED_VALUE => None,
            _ => Some(unsafe { self.start_time }),
        }
    }

    #[inline]
    pub const fn needs_resched(self) -> bool {
        unsafe { self.value == Self::NEED_RESCHED_VALUE }
    }

    pub const fn not_running(&self) -> bool {
        unsafe { self.value == Self::NOT_RUNNING_VALUE }
    }
}

impl fmt::Debug for RunningState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.start_time() {
            Some(st) => write!(f, "Running({:?})", st),
            None => {
                if self.needs_resched() {
                    f.write_str("NeedResched")
                } else {
                    f.write_str("NotRunning")
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct Init {
    pub(in crate::sched) ctx: Box<Context>,
}

impl IntoReady for Init {
    #[inline]
    fn last_cpu(&self) -> Option<usize> {
        None
    }

    #[inline]
    fn affinity(&self) -> CpuMask {
        self.ctx.tid.affinity()
    }

    #[inline]
    fn into_ready(this: Self, cpu: usize, time_slice: Duration) -> Ready {
        let mut ctx = this.ctx;
        ctx.cpu = cpu;
        Ready {
            ctx,
            running_state: RunningState::NOT_RUNNING,
            time_slice,
        }
    }
}

impl Init {
    pub fn new(tid: Tid, space: Arc<Space>, kstack: ctx::Kstack, ext_frame: ctx::ExtFrame) -> Self {
        Init {
            ctx: Box::new(Context {
                tid,
                space,
                kstack,
                ext_frame,
                cpu: 0,
                runtime: Duration::new(0, 0),
            }),
        }
    }

    #[inline]
    pub fn tid(&self) -> &Tid {
        &self.ctx.tid
    }
}

#[derive(Debug)]
pub struct Ready {
    ctx: Box<Context>,

    pub(in crate::sched) running_state: RunningState,
    pub(in crate::sched) time_slice: Duration,
}

pub trait IntoReady {
    fn last_cpu(&self) -> Option<usize>;

    fn affinity(&self) -> CpuMask;

    fn into_ready(this: Self, cpu: usize, time_slice: Duration) -> Ready;
}

impl Ready {
    #[inline]
    pub fn block(this: Self, block_desc: &'static str) -> Blocked {
        Blocked {
            ctx: this.ctx,
            block_desc,
        }
    }

    pub fn exit(this: Self, retval: usize) {
        tid::deallocate(&this.ctx.tid);
        this.ctx.tid.ret_cell.replace(retval);
        idle::CTX_DROPPER.push(this.ctx);
    }
}

impl Deref for Ready {
    type Target = Context;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.ctx
    }
}

impl DerefMut for Ready {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.ctx
    }
}

#[derive(Debug)]
pub struct Blocked {
    ctx: Box<Context>,
    block_desc: &'static str,
}

impl IntoReady for Blocked {
    #[inline]
    fn last_cpu(&self) -> Option<usize> {
        Some(self.ctx.cpu)
    }

    #[inline]
    fn affinity(&self) -> CpuMask {
        self.ctx.tid.affinity()
    }

    #[inline]
    fn into_ready(this: Self, cpu: usize, time_slice: Duration) -> Ready {
        let mut ctx = this.ctx;
        ctx.cpu = cpu;
        Ready {
            ctx,
            running_state: RunningState::NOT_RUNNING,
            time_slice,
        }
    }
}

impl Blocked {
    #[inline]
    pub fn tid(&self) -> &Tid {
        &self.ctx.tid
    }

    #[inline]
    pub fn block_desc(&self) -> &'static str {
        self.block_desc
    }

    #[inline]
    pub fn space(&self) -> &Arc<Space> {
        &self.ctx.space
    }

    #[inline]
    pub fn kstack(&self) -> &ctx::Kstack {
        &self.ctx.kstack
    }

    #[inline]
    pub fn kstack_mut(&mut self) -> &mut ctx::Kstack {
        &mut self.ctx.kstack
    }

    #[inline]
    pub fn ext_frame(&self) -> &ctx::ExtFrame {
        &self.ctx.ext_frame
    }

    #[inline]
    pub fn ext_frame_mut(&mut self) -> &mut ctx::ExtFrame {
        &mut self.ctx.ext_frame
    }
}

#[inline]
pub fn create_entry(entry: LAddr, stack: LAddr, args: [u64; 2]) -> ctx::Entry {
    ctx::Entry { entry, stack, args }
}
