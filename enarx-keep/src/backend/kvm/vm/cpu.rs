// SPDX-License-Identifier: Apache-2.0

use super::x86_64::*;
use super::VirtualMachine;

use crate::backend::kvm::shim::{
    MemInfo, SYSCALL_TRIGGER_PORT, SYS_ENARX_BALLOON_MEMORY, SYS_ENARX_MEM_INFO,
};
use crate::backend::{Command, Thread};
use crate::sallyport::{Block, Reply};

use anyhow::{anyhow, Result};
use kvm_ioctls::{VcpuExit, VcpuFd};
use primordial::{Page, Register};
use x86_64::registers::control::{Cr0Flags, Cr4Flags};
use x86_64::registers::model_specific::EferFlags;
use x86_64::PhysAddr;

use std::sync::{Arc, RwLock};

pub struct Allocator {
    next_cpu: usize,
    _reclaimed: Vec<usize>,
}

impl Allocator {
    pub fn new() -> Self {
        Self {
            next_cpu: 0,
            _reclaimed: vec![],
        }
    }

    pub fn next(&mut self) -> usize {
        let allocated = self.next_cpu;
        self.next_cpu += 1;
        allocated
    }
}

pub struct Cpu {
    fd: VcpuFd,
    id: usize,
    keep: Arc<RwLock<VirtualMachine>>,
}

impl Cpu {
    pub fn new(
        fd: VcpuFd,
        id: usize,
        keep: Arc<RwLock<VirtualMachine>>,
        entry: PhysAddr,
        cr3: u64,
    ) -> Result<Self> {
        let mut cpu = Self { fd, id, keep };

        cpu.set_gen_regs(entry)?;
        cpu.set_special_regs(cr3)?;

        Ok(cpu)
    }

    fn set_gen_regs(&mut self, entry: PhysAddr) -> Result<()> {
        let mut regs = self.fd.get_regs()?;

        regs.rip = entry.as_u64();
        regs.rflags |= 0x2;

        self.fd.set_regs(&regs)?;
        Ok(())
    }

    fn set_special_regs(&mut self, cr3: u64) -> Result<()> {
        let mut sregs = self.fd.get_sregs()?;

        let cs = KvmSegment {
            base: 0,
            limit: 0xFFFFF,
            selector: 8,
            type_: 11,
            present: 1,
            dpl: 0,
            db: 0,
            s: 1,
            l: 1,
            g: 1,
            avl: 0,
            unusable: 0,
            padding: 0,
        };

        sregs.cs = cs;

        sregs.efer = (EferFlags::LONG_MODE_ENABLE | EferFlags::LONG_MODE_ACTIVE).bits();
        sregs.cr0 = (Cr0Flags::PROTECTED_MODE_ENABLE
            | Cr0Flags::NUMERIC_ERROR
            | Cr0Flags::PAGING
            | Cr0Flags::MONITOR_COPROCESSOR)
            .bits();
        sregs.cr3 = cr3;
        sregs.cr4 = (Cr4Flags::PHYSICAL_ADDRESS_EXTENSION).bits();

        self.fd.set_sregs(&sregs)?;
        Ok(())
    }
}

impl Thread for Cpu {
    fn enter(&mut self) -> Result<Command> {
        match self.fd.run()? {
            VcpuExit::IoOut(port, _) => match port {
                SYSCALL_TRIGGER_PORT => {
                    let mut keep = self.keep.write().unwrap();
                    let mut sallyport = {
                        let page = keep.regions[0]
                            .prefix_mut()
                            .shared_pages
                            .get_mut(self.id)
                            .unwrap();

                        unsafe { &mut *(page as *mut Page as *mut Block) }
                    };
                    let syscall_nr: i64 = unsafe { sallyport.msg.req.num.into() };

                    match syscall_nr {
                        0..=512 => Ok(Command::SysCall(sallyport)),

                        SYS_ENARX_BALLOON_MEMORY => {
                            let pages = unsafe { sallyport.msg.req.arg[0].into() };

                            let result = keep.add_memory(pages).map(|addr| {
                                let ok_result: [Register<usize>; 2] = [addr.into(), 0.into()];
                                ok_result
                            })?;

                            sallyport.msg.rep = Reply::from(Ok(result));
                            Ok(Command::Continue)
                        }
                        SYS_ENARX_MEM_INFO => {
                            let mem_slots = keep.kvm.get_nr_memslots();
                            let virt_offset: i64 =
                                keep.regions.first().unwrap().as_virt().start.as_u64() as _;
                            let mem_info: MemInfo = MemInfo {
                                virt_offset,
                                mem_slots,
                            };

                            let c = sallyport.cursor();
                            let (_, buf) = unsafe {
                                c.alloc::<MemInfo>(1)
                                    .map_err(|_| anyhow!("Failed to allocate MemInfo in Block"))?
                            };

                            buf[0] = mem_info;

                            let ok_result: [Register<usize>; 2] = [0.into(), 0.into()];

                            sallyport.msg.rep = Reply::from(Ok(ok_result));

                            Ok(Command::Continue)
                        }

                        _ => unimplemented!(),
                    }
                }
                _ => Err(anyhow!("data from unexpected port: {}", port)),
            },
            exit_reason => Err(anyhow!("{:?}", exit_reason)),
        }
    }
}
