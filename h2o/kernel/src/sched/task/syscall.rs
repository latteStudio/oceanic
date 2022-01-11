use alloc::{string::String, sync::Arc, vec::Vec};
use core::{hint, slice, time::Duration};

use paging::LAddr;
use scopeguard::defer;
use solvent::*;

use super::{RunningState, Signal, Tid, DEFAULT_STACK_SIZE};
use crate::{
    cpu::time::Instant,
    mem::space,
    sched::{wait::WaitObject, SCHED},
    syscall::{In, InOut, UserPtr},
};

#[derive(Debug)]
struct SuspendToken {
    wo: Arc<WaitObject>,
    tid: Tid,
}

impl SuspendToken {
    #[inline]
    pub fn signal(&self) -> Signal {
        Signal::Suspend(Arc::clone(&self.wo))
    }
}

impl Drop for SuspendToken {
    fn drop(&mut self) {
        if self.wo.notify(1) == 0 {
            self.tid.update_signal(|sig| {
                if matches!(sig, Some(sig) if sig == &mut self.signal()) {
                    *sig = None;
                }
            })
        }
    }
}

#[syscall]
fn task_exit(retval: usize) {
    SCHED.exit_current(retval);
}

#[syscall]
fn task_sleep(ms: u32) {
    if ms == 0 {
        SCHED.with_current(|cur| cur.running_state = RunningState::NeedResched);
        SCHED.tick(Instant::now());
    } else {
        SCHED.block_current((), None, Duration::from_millis(u64::from(ms)), "task_sleep");
    }
    Ok(())
}

#[syscall]
fn task_fn(ci: UserPtr<In, task::CreateInfo>) -> u32 {
    let ci = unsafe { ci.read()? };

    let name = {
        let ptr = UserPtr::<In, _>::new(ci.name);
        if !ptr.as_ptr().is_null() {
            let mut buf = Vec::<u8>::with_capacity(ci.name_len);
            unsafe { ptr.read_slice(buf.as_mut_ptr(), buf.len()) }?;
            Some(String::from_utf8(buf).map_err(|_| Error(EINVAL))?)
        } else {
            None
        }
    };

    let stack_size = if ci.stack_size == 0 {
        DEFAULT_STACK_SIZE
    } else {
        ci.stack_size
    };

    let init_chan = match ci.init_chan.check_null() {
        Ok(hdl) => SCHED.with_current(|cur| {
            let mut map = cur.tid().handles().write();
            map.remove::<crate::sched::ipc::Channel>(hdl)
                .ok_or(Error(EINVAL))
        }),
        Err(_) => None,
    }
    .transpose()?;

    UserPtr::<In, _>::new(ci.func).check()?;

    let (task, ret_wo) = super::create_fn(name, stack_size, init_chan, LAddr::new(ci.func), ci.arg)
        .map_err(Into::into)?;
    SCHED.push(task);

    Ok(ret_wo.raw())
}

#[syscall]
fn task_join(hdl: Handle) -> usize {
    hdl.check_null()?;

    let child = {
        let tid = SCHED
            .with_current(|cur| cur.tid.clone())
            .ok_or(Error(ESRCH))?;

        tid.child(hdl).ok_or(Error(ECHILD))?
    };

    Error::decode(child.cell().take(Duration::MAX, "task_join").unwrap())
}

#[syscall]
fn task_ctl(hdl: Handle, op: u32, data: UserPtr<InOut, Handle>) {
    hdl.check_null()?;

    let cur_tid = SCHED
        .with_current(|cur| cur.tid.clone())
        .ok_or(Error(ESRCH))?;

    match op {
        task::TASK_CTL_KILL => {
            let child = cur_tid.child(hdl).ok_or(Error(ECHILD))?;
            child.tid().replace_signal(Some(Signal::Kill));

            Ok(())
        }
        task::TASK_CTL_SUSPEND => {
            data.out().check()?;

            let child = cur_tid.child(hdl).ok_or(Error(ECHILD))?;

            let st = SuspendToken {
                wo: Arc::new(WaitObject::new()),
                tid: child.tid().clone(),
            };

            st.tid.replace_signal(Some(st.signal()));

            let out = {
                let _pree = super::PREEMPT.lock();
                cur_tid.handles().write().insert(st)
            };
            unsafe { data.out().write(out) }.unwrap();

            Ok(())
        }
        task::TASK_CTL_DETACH => {
            if cur_tid.drop_child(hdl) {
                Ok(())
            } else {
                Err(Error(ECHILD))
            }
        }
        _ => Err(Error(EINVAL)),
    }
}

#[syscall]
fn task_debug(hdl: Handle, op: u32, addr: usize, data: UserPtr<InOut, u8>, len: usize) {
    hdl.check_null()?;

    if len < core::mem::size_of::<usize>() {
        return Err(Error(EINVAL));
    }
    data.check_slice(len)?;

    let wo = SCHED
        .with_current(|cur| {
            let handles = cur.tid.handles.read();
            match handles.get::<SuspendToken>(hdl) {
                Some(st) => Some(Arc::clone(&st.wo)),
                None => None,
            }
        })
        .ok_or(Error(ESRCH))?
        .ok_or(Error(EINVAL))?;

    let timer = loop {
        match wo.wait_queue.steal() {
            crate::sched::deque::Steal::Success(timer) => break timer,
            _ => hint::spin_loop(),
        }
    };
    let task = timer.callback_arg().cast::<super::Blocked>();
    defer! { wo.wait_queue.push(timer); }

    match op {
        task::TASK_DBG_READ_MEM => unsafe {
            space::with(&(*task).space, |_| {
                let slice = slice::from_raw_parts(addr as *mut u8, len);
                data.out().write_slice(slice)
            })
        },
        task::TASK_DBG_WRITE_MEM => unsafe {
            space::with(&(*task).space, |_| {
                data.r#in().read_slice(addr as *mut u8, len)
            })
        },
        _ => Err(Error(EINVAL)),
    }
}
