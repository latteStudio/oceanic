pub mod hdl;
#[cfg(feature = "call")]
pub mod raw;
pub mod reg;

use solvent_gen::syscall_stub as ss;

#[allow(unused_imports)]
use crate::{ipc::RawPacket, Arguments, Handle, SerdeReg};

ss!(0 => pub fn get_time(ptr: *mut u128));
// #[cfg(debug_assertions)]
ss!(1 => pub fn log(args: *const ::log::Record));

ss!(2 => pub fn task_exit(retval: usize));
ss!(3 => pub fn task_exec(ci: *const crate::task::ExecInfo) -> Handle);
ss!(4 =>
    pub fn task_new(
        name: *const u8,
        name_len: usize,
        space: Handle,
        init: *mut Handle
    ) -> Handle
);
ss!(5 => pub fn task_join(hdl: Handle) -> usize);
ss!(6 => pub fn task_ctl(hdl: Handle, op: u32, data: *mut Handle));
ss!(7 =>
    pub fn task_debug(
        hdl: Handle,
        op: u32,
        addr: usize,
        data: *mut u8,
        len: usize
    )
);
ss!(8 => pub fn task_sleep(ms: u32));

ss!(10 => pub fn phys_alloc(size: usize, align: usize, flags: u32) -> Handle);
ss!(11 => pub fn mem_new() -> Handle);
ss!(12 => pub fn mem_map(space: Handle, mi: *const crate::mem::MapInfo) -> *mut u8);
ss!(13 => pub fn mem_reprot(space: Handle, ptr: *mut u8, len: usize, flags: u32));
ss!(14 => pub fn mem_unmap(space: Handle, ptr: *mut u8));

ss!(16 => pub fn futex_wait(ptr: *mut u64, expected: u64, timeout_us: u64) -> bool);
ss!(17 => pub fn futex_wake(ptr: *mut u64, num: usize) -> usize);
ss!(18 =>
    pub fn futex_reque(
        ptr: *mut u64,
        wake_num: *mut usize,
        other: *mut u64,
        requeue_num: *mut usize,
    )
);

ss!(20 => pub fn obj_clone(hdl: Handle) -> Handle);
ss!(21 => pub fn obj_drop(hdl: Handle));

ss!(23 => pub fn chan_new(p1: *mut Handle, p2: *mut Handle));
ss!(24 => pub fn chan_send(hdl: Handle, packet: *const RawPacket));
ss!(25 => pub fn chan_recv(hdl: Handle, packet: *mut RawPacket, timeout_us: u64));
ss!(26 => pub fn chan_csend(hdl: Handle, packet: *const RawPacket) -> usize);
ss!(27 => pub fn chan_crecv(hdl: Handle, id: usize, packet: *mut RawPacket, timeout_us: u64));

ss!(29 => pub fn event_new(wake_all: bool) -> Handle);
ss!(30 => pub fn event_wait(hdl: Handle, signal: u8, timeout_us: u64));
ss!(31 => pub fn event_notify(hdl: Handle, active: u8) -> usize);
ss!(32 => pub fn event_endn(hdl: Handle, masked: u8));

ss!(34 => pub fn intr_new(res: Handle, gsi: u32, config: u32) -> Handle);
ss!(35 => pub fn intr_wait(hdl: Handle, timeout_us: u64, last_time: *mut u128));
ss!(36 => pub fn intr_drop(hdl: Handle));

ss!(38 => pub fn res_alloc(hdl: Handle, base: usize, size: usize) -> Handle);
ss!(39 =>
    pub fn phys_acq(
        res: Handle,
        addr: usize,
        size: usize,
        align: usize,
        flags: u32
    ) -> Handle
);
ss!(40 => pub fn pio_acq(res: Handle, base: u16, size: u16));
ss!(41 => pub fn pio_rel(res: Handle, base: u16, size: u16));
