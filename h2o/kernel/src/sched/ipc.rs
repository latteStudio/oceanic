mod arsc;
pub mod basic;
mod channel;

use alloc::{sync::Arc, vec::Vec};
use core::{
    fmt::Debug,
    hint, mem,
    sync::atomic::{AtomicUsize, Ordering::SeqCst},
};

use spin::Mutex;
pub use sv_call::ipc::{SIG_GENERIC, SIG_READ, SIG_TIMER, SIG_WRITE};

pub use self::{
    arsc::Arsc,
    channel::{Channel, Packet},
};
use super::PREEMPT;
use crate::cpu::arch::apic::TriggerMode;

#[derive(Debug, Default)]
pub struct EventData {
    waiters: Mutex<Vec<Arc<dyn Waiter>>>,
    signal: AtomicUsize,
}

impl EventData {
    pub fn new(init_signal: usize) -> Self {
        EventData {
            waiters: Mutex::new(Vec::new()),
            signal: AtomicUsize::new(init_signal),
        }
    }

    #[inline]
    pub fn waiters(&self) -> &Mutex<Vec<Arc<dyn Waiter>>> {
        &self.waiters
    }

    #[inline]
    pub fn signal(&self) -> &AtomicUsize {
        &self.signal
    }
}

pub trait Event: Debug + Send + Sync {
    fn event_data(&self) -> &EventData;

    #[inline]
    fn wait(&self, waiter: Arc<dyn Waiter>) {
        self.wait_impl(waiter);
    }

    fn wait_impl(&self, waiter: Arc<dyn Waiter>) {
        let signal = self.event_data().signal().load(SeqCst);
        if waiter.try_on_notify(self as *const _ as _, signal, true) {
            return;
        }
        PREEMPT.scope(|| self.event_data().waiters.lock().push(waiter));
    }

    fn unwait(&self, waiter: &Arc<dyn Waiter>) -> (bool, usize) {
        let signal = self.event_data().signal().load(SeqCst);
        let ret = PREEMPT.scope(|| {
            let mut waiters = self.event_data().waiters.lock();
            let pos = waiters.iter().position(|w| {
                let (this, _) = Arc::as_ptr(w).to_raw_parts();
                let (other, _) = Arc::as_ptr(waiter).to_raw_parts();
                this == other
            });
            match pos {
                Some(pos) => {
                    waiters.swap_remove(pos);
                    true
                }
                None => false,
            }
        });
        (ret, signal)
    }

    fn cancel(&self) {
        let signal = self.event_data().signal.load(SeqCst);

        let waiters = PREEMPT.scope(|| mem::take(&mut *self.event_data().waiters.lock()));
        for waiter in waiters {
            waiter.on_cancel(self as *const _ as _, signal);
        }
    }

    #[inline]
    fn notify(&self, clear: usize, set: usize) {
        self.notify_impl(clear, set);
    }

    fn notify_impl(&self, clear: usize, set: usize) {
        let signal = loop {
            let prev = self.event_data().signal.load(SeqCst);
            let new = (prev & !clear) | set;
            if prev == new {
                return;
            }
            match self
                .event_data()
                .signal
                .compare_exchange_weak(prev, new, SeqCst, SeqCst)
            {
                Ok(_) if prev & new == new => return,
                Ok(_) => break new,
                _ => hint::spin_loop(),
            }
        };
        PREEMPT.scope(|| {
            let mut waiters = self.event_data().waiters.lock();
            waiters.retain(|waiter| !waiter.try_on_notify(self as *const _ as _, signal, false))
        });
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WaiterData {
    trigger_mode: TriggerMode,
    signal: usize,
}

impl WaiterData {
    pub fn new(trigger_mode: TriggerMode, signal: usize) -> Self {
        WaiterData {
            trigger_mode,
            signal,
        }
    }

    pub fn trigger_mode(&self) -> TriggerMode {
        self.trigger_mode
    }

    pub fn signal(&self) -> usize {
        self.signal
    }

    #[inline]
    pub fn can_signal(&self, signal: usize, on_wait: bool) -> bool {
        if on_wait && self.trigger_mode == TriggerMode::Edge {
            false
        } else {
            self.signal & !signal == 0
        }
    }
}

pub trait Waiter: Debug + Send + Sync {
    fn waiter_data(&self) -> WaiterData;

    fn on_cancel(&self, event: *const (), signal: usize);

    fn on_notify(&self, signal: usize);

    #[inline]
    fn try_on_notify(&self, _: *const (), signal: usize, on_wait: bool) -> bool {
        let ret = self.waiter_data().can_signal(signal, on_wait);
        if ret {
            self.on_notify(signal);
        }
        ret
    }
}

mod syscall {
    use sv_call::*;

    use super::*;
    use crate::{
        cpu::time,
        sched::{Blocker, Dispatcher, SCHED},
        syscall::{Out, UserPtr},
    };

    #[syscall]
    fn obj_wait(hdl: Handle, timeout_us: u64, wake_all: bool, signal: usize) -> Result<usize> {
        let pree = PREEMPT.lock();
        let cur = unsafe { (*SCHED.current()).as_ref().ok_or(ESRCH) }?;

        let obj = cur.space().handles().get_ref(hdl)?;
        if !obj.features().contains(Feature::WAIT) {
            return Err(EPERM);
        }
        let event = obj.event().upgrade().ok_or(EPIPE)?;

        let blocker = Blocker::new(&event, wake_all, signal);
        blocker.wait(Some(pree), time::from_us(timeout_us))?;

        let (detach_ret, signal) = blocker.detach();
        if !detach_ret {
            return Err(ETIME);
        }
        Ok(signal)
    }

    #[syscall]
    fn obj_await(hdl: Handle, wake_all: bool, signal: usize) -> Result<Handle> {
        hdl.check_null()?;
        SCHED.with_current(|cur| {
            let obj = cur.space().handles().get_ref(hdl)?;
            if !obj.features().contains(Feature::WAIT) {
                return Err(EPERM);
            }
            let event = obj.event().upgrade().ok_or(EPIPE)?;

            let blocker = Blocker::new(&event, wake_all, signal);
            cur.space().handles().insert_raw(blocker, None)
        })
    }

    #[syscall]
    fn obj_awend(waiter: Handle, timeout_us: u64) -> Result<usize> {
        let pree = PREEMPT.lock();
        let cur = unsafe { (*SCHED.current()).as_ref().ok_or(ESRCH) }?;

        let blocker = cur.space().handles().get::<Blocker>(waiter)?;
        blocker.wait(Some(pree), time::from_us(timeout_us))?;

        let (detach_ret, signal) = Arc::clone(&blocker).detach();
        SCHED.with_current(|cur| cur.space().handles().remove::<Blocker>(waiter))?;

        if !detach_ret {
            Err(ETIME)
        } else {
            Ok(signal)
        }
    }

    #[syscall]
    fn disp_new() -> Result<Handle> {
        let disp = Dispatcher::new();
        let event = disp.event();
        SCHED.with_current(|cur| cur.space().handles().insert(disp, Some(event)))
    }

    #[syscall]
    fn obj_await2(
        hdl: Handle,
        level_triggered: bool,
        signal: usize,
        disp: Handle,
    ) -> Result<usize> {
        hdl.check_null()?;
        disp.check_null()?;
        SCHED.with_current(|cur| {
            let obj = cur.space().handles().get_ref(hdl)?;
            let disp = cur.space().handles().get::<Dispatcher>(disp)?;
            if !obj.features().contains(Feature::WAIT) {
                return Err(EPERM);
            }
            if !disp.features().contains(Feature::WRITE) {
                return Err(EPERM);
            }
            let event = obj.event().upgrade().ok_or(EPIPE)?;

            let waiter_data = WaiterData::new(
                if level_triggered {
                    TriggerMode::Level
                } else {
                    TriggerMode::Edge
                },
                signal,
            );
            Ok(disp.push(&event, waiter_data))
        })
    }

    #[syscall]
    fn obj_awend2(disp: Handle, canceled: UserPtr<Out, bool>) -> Result<usize> {
        disp.check_null()?;
        canceled.check()?;
        SCHED.with_current(|cur| {
            let disp = cur.space().handles().get::<Dispatcher>(disp)?;
            if !disp.features().contains(Feature::READ) {
                return Err(EPERM);
            }
            let (key, c) = disp.pop().ok_or(ENOENT)?;
            canceled.write(c)?;
            Ok(key)
        })
    }

    mod asd {
        use sv_call::{call::Syscall, *};

        use crate::{
            cpu::arch::apic::TriggerMode,
            sched::{imp::disp::Dispatcher, WaiterData, SCHED},
            syscall::{In, Out, UserPtr},
        };

        #[syscall]
        fn disp_new2(capacity: usize) -> Result<Handle> {
            let disp = Dispatcher::new(capacity)?;
            let event = disp.event();
            SCHED.with_current(|cur| cur.space().handles().insert_raw(disp, Some(event)))
        }

        #[syscall]
        fn disp_push(
            disp: Handle,
            hdl: Handle,
            level_triggered: bool,
            signal: usize,
            syscall: UserPtr<In, Syscall>,
        ) -> Result<usize> {
            hdl.check_null()?;
            disp.check_null()?;
            let syscall = unsafe { syscall.read()? };
            // if syscall.num == 
            SCHED.with_current(|cur| {
                let obj = cur.space().handles().get_ref(hdl)?;
                let disp = cur.space().handles().get::<Dispatcher>(disp)?;
                if !obj.features().contains(Feature::WAIT) {
                    return Err(EPERM);
                }
                if !disp.features().contains(Feature::WRITE) {
                    return Err(EPERM);
                }
                let event = obj.event().upgrade().ok_or(EPIPE)?;

                let waiter_data = WaiterData::new(
                    if level_triggered {
                        TriggerMode::Level
                    } else {
                        TriggerMode::Edge
                    },
                    signal,
                );
                disp.push(&event, waiter_data, syscall)
            })
        }

        #[syscall]
        fn disp_pop(
            disp: Handle,
            canceled: UserPtr<Out, bool>,
            result: UserPtr<Out, usize>,
        ) -> Result<usize> {
            disp.check_null()?;
            canceled.check()?;
            result.check()?;
            SCHED.with_current(|cur| {
                let disp = cur.space().handles().get::<Dispatcher>(disp)?;
                if !disp.features().contains(Feature::READ) {
                    return Err(EPERM);
                }
                let (c, key, r) = disp.pop().ok_or(ENOENT)?;
                canceled.write(c)?;
                result.write(r)?;
                Ok(key)
            })
        }
    }
}
