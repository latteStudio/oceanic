#![no_std]
#![no_main]
#![allow(unused_unsafe)]
#![feature(alloc_error_handler)]
#![feature(alloc_layout_extra)]
#![feature(box_syntax)]
#![feature(int_roundings)]
#![feature(lang_items)]
#![feature(min_specialization)]
#![feature(nonnull_slice_from_raw_parts)]
#![feature(result_option_inspect)]
#![feature(slice_ptr_get)]
#![feature(slice_ptr_len)]
#![feature(thread_local)]
#![feature(vec_into_raw_parts)]

mod load;
mod mem;
mod rxx;
mod test;

use alloc::{ffi::CString, vec, vec::Vec};
use core::{hint, mem::MaybeUninit, time::Duration};

use bootfs::parse::Directory;
use solvent::prelude::*;
use solvent_rpc::{loader::GET_OBJECT, packet};
use sv_call::ipc::SIG_READ;
use svrt::{HandleType, StartupArgs};
use targs::{HandleIndex, Targs};

extern crate alloc;

static mut ROOT_VIRT: MaybeUninit<Virt> = MaybeUninit::uninit();

fn is_sub<T>(slice: &[T], parent: &[T]) -> bool {
    let range = parent.as_ptr_range();
    let srange = slice.as_ptr_range();
    range.start <= srange.start && srange.end <= range.end
}

fn offset_sub<T>(slice: &[T], parent: &[T]) -> Option<usize> {
    is_sub(slice, parent).then(|| {
        let start = parent.as_ptr();
        let end = slice.as_ptr();
        // SAFETY: `slice` is part of `parent`
        unsafe { end.offset_from(start) as usize }
    })
}

fn sub_phys(bin_data: &[u8], bootfs: Directory, bootfs_phys: &Phys) -> Result<Phys> {
    let offset = offset_sub(bin_data, bootfs.image()).ok_or(ERANGE)?;
    bootfs_phys.create_sub(offset, bin_data.len().next_multiple_of(PAGE_SIZE), false)
}

fn map_bootfs(phys: &Phys, root: &Virt) -> Directory<'static> {
    let ptr = root
        .map_phys(
            None,
            Phys::clone(phys),
            Flags::READABLE | Flags::EXECUTABLE | Flags::USER_ACCESS,
        )
        .expect("Failed to map boot FS");
    Directory::root(unsafe { ptr.as_ref() }).expect("Failed to parse boot filesystem")
}

fn serve_load(load_rpc: Channel, bootfs: Directory, bootfs_phys: &Phys) -> Error {
    loop {
        let res = load_rpc.handle(|packet| {
            let paths: Vec<CString> =
                packet::deserialize(GET_OBJECT, packet, None).map_err(|_| solvent::error::ETYPE)?;
            let response = {
                let mut objs = Vec::with_capacity(paths.len());
                let mut err = None;
                for (i, path) in paths.into_iter().enumerate() {
                    let mut root = Vec::from(b"lib/" as &[u8]);
                    root.append(&mut path.into_bytes());
                    let obj = bootfs
                        .find(&root, b'/')
                        .and_then(|bin| sub_phys(bin, bootfs, bootfs_phys).ok());
                    match obj {
                        Some(obj) => objs.push(obj),
                        None => {
                            err = Some(i);
                            break;
                        }
                    }
                }
                match err {
                    Some(err) => Err(err),
                    None => Ok(objs),
                }
            };
            packet::serialize(GET_OBJECT, response, packet).map_err(|_| solvent::error::EFAULT)?;
            Ok(())
        });

        match res {
            Ok(()) => hint::spin_loop(),
            Err(ENOENT) => match load_rpc.try_wait(Duration::MAX, true, true, SIG_READ) {
                Ok(_) => {}
                Err(err) => break err,
            },
            Err(err) => break err,
        }
    }
}

#[no_mangle]
extern "C" fn tmain(init_chan: sv_call::Handle) {
    dbglog::init(log::Level::Debug);
    log::info!("Starting initialization");

    let init_chan = unsafe { Channel::from_raw(init_chan) };
    let mut buffer = [0; core::mem::size_of::<Targs>()];
    let mut handles = [MaybeUninit::uninit(); HandleIndex::Len as usize];
    init_chan
        .receive_raw(&mut buffer, &mut handles)
        .0
        .expect("Failed to receive the initial packet");

    let _targs = {
        let mut targs = Targs::default();
        plain::copy_from_bytes(&mut targs, &buffer).expect("Failed to get TINIT args");
        targs
    };

    let root_virt = unsafe {
        &*ROOT_VIRT.write(Virt::from_raw(
            handles[HandleIndex::RootVirt as usize].assume_init(),
        ))
    };

    mem::init();

    unsafe { test::test_syscall(root_virt) };

    let vdso_phys = unsafe { Phys::from_raw(handles[HandleIndex::Vdso as usize].assume_init()) };

    let bootfs_phys =
        unsafe { Phys::from_raw(handles[HandleIndex::Bootfs as usize].assume_init()) };
    let bootfs = map_bootfs(&bootfs_phys, root_virt);

    let bin = {
        let bin_data = bootfs
            .find(b"bin/progm", b'/')
            .expect("Failed to find progm");

        sub_phys(bin_data, bootfs, &bootfs_phys).expect("Failed to create the physical object")
    };

    let (space, virt) = Space::new();
    let (entry, stack) =
        load::load_elf(&bin, bootfs, &bootfs_phys, &virt).expect("Failed to load test_bin");

    let vdso_base = virt
        .map_vdso(Phys::clone(&vdso_phys))
        .expect("Failed to load VDSO");
    log::debug!("E {:?}; S {:?}; V {:?}", entry, stack, vdso_base);

    let (me, child) = Channel::new();
    let child = child
        .reduce_features(Feature::SEND | Feature::READ)
        .expect("Failed to reduce features for read");
    let me = me
        .reduce_features(Feature::SEND | Feature::WRITE)
        .expect("Failed to reduce features for write");

    let load_rpc = Channel::new();

    let dl_args = StartupArgs {
        handles: [
            (HandleType::RootVirt.into(), Virt::into_raw(virt.clone())),
            (
                HandleType::VdsoPhys.into(),
                Phys::into_raw(vdso_phys.clone()),
            ),
            (HandleType::ProgramPhys.into(), Phys::into_raw(bin)),
            (HandleType::LoadRpc.into(), Channel::into_raw(load_rpc.1)),
        ]
        .into_iter()
        .collect(),
        args: vec![0],
        env: vec![0],
    };

    let mut packet = Default::default();
    dl_args
        .send(&me, &mut packet)
        .expect("Failed to send dyn loader args");

    let exe_args = StartupArgs {
        handles: [
            (HandleType::RootVirt.into(), Virt::into_raw(virt)),
            (HandleType::VdsoPhys.into(), Phys::into_raw(vdso_phys)),
            (
                HandleType::BootfsPhys.into(),
                Phys::into_raw(bootfs_phys.clone()),
            ),
        ]
        .into_iter()
        .collect(),
        args: Vec::from(b"progm\0" as &[u8]),
        env: vec![0],
    };

    exe_args
        .send(&me, &mut packet)
        .expect("Failed to send executable args");
    drop(me);

    let task = Task::exec(
        Some("PROGMGR"),
        Some(space),
        entry,
        stack,
        Some(child),
        vdso_base.as_mut_ptr() as u64,
    )
    .expect("Failed to create the task");

    log::debug!("Serving for load_rpc");
    let err = serve_load(load_rpc.0, bootfs, &bootfs_phys);
    log::debug!("End service for load_rpc: {:?}", err);

    log::debug!("Waiting for the task");

    let retval = task.join().expect("Failed to join the task");
    log::debug!("{:x} {:?}", retval, Error::try_from_retval(retval));

    log::debug!("Reaching end of TINIT");
}
