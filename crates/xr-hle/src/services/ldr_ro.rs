//! ldr:ro, the dynamic module loader

use crate::cro::CroError;
use crate::kernel::ipc::{CommandBuffer, Header};
use crate::System;
use zakuro_cpu::Bus;

mod command {
    pub const INITIALIZE: u16 = 0x0001;
    pub const LOAD_CRR: u16 = 0x0002;
    pub const UNLOAD_CRR: u16 = 0x0003;
    pub const LOAD_CRO: u16 = 0x0004;
    pub const UNLOAD_CRO: u16 = 0x0005;
    pub const LINK_CRO: u16 = 0x0006;
    pub const UNLINK_CRO: u16 = 0x0007;
    pub const SHUTDOWN: u16 = 0x0008;
    pub const LOAD_CRO_NEW: u16 = 0x0009;
}

/// 0xD9012402, the module isn't a valid CRO
const ERROR_NOT_LOADED: u32 = 0xD901_2402;

pub fn handle(system: &mut System, buffer: &CommandBuffer, header: Header) -> bool {
    use command::*;
    let id = header.command_id();
    match id {
        // Initialize(crs buffer, size, addr, <process>)
        INITIALIZE => {
            let source = buffer.get(&mut system.memory, 1);
            let size = buffer.get(&mut system.memory, 2);
            let address = buffer.get(&mut system.memory, 3);

            let base = place(system, address, source, size);
            match system.cro.initialize(&mut system.memory, base, size) {
                Ok(()) => buffer.reply(&mut system.memory, id, &[]),
                Err(error) => {
                    log::error!("ldr:ro Initialize failed: {error:?}");
                    buffer.reply_error(&mut system.memory, id, ERROR_NOT_LOADED);
                }
            }
            true
        }

        // the certificate tables are a signature check we don't do
        LOAD_CRR | UNLOAD_CRR => {
            buffer.reply(&mut system.memory, id, &[]);
            true
        }

        // LoadCRO(buffer, addr, size, data segment, 0, data size, bss segment, bss size, auto link, fix level, crr, <process>)
        LOAD_CRO | LOAD_CRO_NEW => {
            let source = buffer.get(&mut system.memory, 1);
            let address = buffer.get(&mut system.memory, 2);
            let size = buffer.get(&mut system.memory, 3);
            let data_segment = buffer.get(&mut system.memory, 4);
            let data_segment_size = buffer.get(&mut system.memory, 6);
            let bss_segment = buffer.get(&mut system.memory, 7);
            let bss_segment_size = buffer.get(&mut system.memory, 8);
            // a u8 in its word
            let auto_link = buffer.get(&mut system.memory, 9) & 0xFF != 0;
            let fix_level = buffer.get(&mut system.memory, 10);

            log::debug!(
                "ldr:ro LoadCRO: buffer 0x{source:08X} -> 0x{address:08X}, 0x{size:X} bytes, \
                 data 0x{data_segment:08X}+0x{data_segment_size:X}, \
                 bss 0x{bss_segment:08X}+0x{bss_segment_size:X}, auto link {auto_link}"
            );
            let base = copy_in(system, address, source, size);
            // xr-hle: the code segment as loaded (segment 0), hashed before relocation changes it
            let code = system.module_hook.map(|_| {
                let table = system.memory.read32(base + crate::cro::format::header::SEGMENT_TABLE_OFFSET);
                let (offset, len) = (system.memory.read32(base + table), system.memory.read32(base + table + 4));
                let mut bytes = vec![0u8; len.min(size) as usize];
                system.memory.read_bytes(base + offset, &mut bytes);
                (base + offset, bytes.len() as u32, crate::cro::code_hash(&bytes))
            });
            let result = system.cro.load(
                &mut system.memory,
                base,
                size,
                data_segment,
                data_segment_size,
                bss_segment,
                bss_segment_size,
                auto_link,
                fix_level,
            );
            match result {
                Ok(fix_size) => {
                    copy_back(system, base, size);
                    shrink(system, base, if fix_size == 0 { size } else { fix_size });
                    // xr-hle: where a module landed, for native CRO code later (study §4) and the LoadCRO address log
                    if let Some(module) = system.cro.modules.last() {
                        log::info!("ldr:ro: {} placed at 0x{:08X}", module.name, module.base);
                        let segments: Vec<String> = module.segments.iter().map(|s| format!("0x{s:08X}")).collect();
                        let row = format!(
                            "{},0x{source:08X},0x{address:08X},0x{:08X},0x{size:X},0x{data_segment:08X},0x{data_segment_size:X},0x{bss_segment:08X},0x{bss_segment_size:X},{auto_link},{fix_level},0x{fix_size:X},{}",
                            module.name, module.base, segments.join(" ")
                        );
                        log_load(system, &row);
                        if let (Some(hook), Some(code)) = (system.module_hook, code) {
                            hook(&module.name, Some(code));
                        }
                    }
                    buffer.reply(&mut system.memory, id, &[fix_size])
                }
                Err(CroError::NotACro) => {
                    log::error!("ldr:ro: the buffer at 0x{source:08X} is not a CRO");
                    buffer.reply_error(&mut system.memory, id, ERROR_NOT_LOADED);
                }
                Err(error) => {
                    log::error!("ldr:ro LoadCRO failed: {error:?}");
                    buffer.reply_error(&mut system.memory, id, ERROR_NOT_LOADED);
                }
            }
            true
        }

        UNLOAD_CRO => {
            let address = buffer.get(&mut system.memory, 1);
            if let Some(module) = system.cro.modules.iter().find(|m| m.base == address) {
                log::info!("ldr:ro: {} unloaded from 0x{address:08X}", module.name);
                if let Some(hook) = system.module_hook {
                    hook(&module.name, None);
                }
            }
            system.cro.unload(&mut system.memory, address);
            copy_out(system, address);
            buffer.reply(&mut system.memory, id, &[]);
            true
        }

        // a module gets linked as it loads, so these have nothing left to do
        LINK_CRO | UNLINK_CRO | SHUTDOWN => {
            buffer.reply(&mut system.memory, id, &[]);
            true
        }

        _ => false,
    }
}

/// xr-hle: appends a LoadCRO row to `<data>/logs/loadcro.csv` (the runtime clears it at boot), for deciding which
/// modules to recompile ahead of time
fn log_load(system: &System, row: &str) {
    use std::io::Write;
    let Some(dir) = system.config.data_dir.as_ref().filter(|_| !system.config.no_cro_log) else { return };
    let path = dir.join("logs/loadcro.csv");
    let header = !path.exists();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        if header {
            let _ = writeln!(f, "module,buffer,map_address,base,size,data,data_size,bss,bss_size,auto_link,fix_level,fix_size,segments");
        }
        let _ = writeln!(f, "{row}");
    }
}

/// gives a module its own memory at the addr the title wants to run it from, a copy of its buffer, and returns that addr
/// ldr:ro takes the buffer's pages away from the title while the module's loaded,
/// so whatever the title writes to the buffer meanwhile (like a module's data it puts back on unloading) must not
/// reach a module running from memory the title has since handed to another
fn copy_in(system: &mut System, address: u32, source: u32, size: u32) -> u32 {
    use crate::memory::{MemoryRegion, MemoryState, Permission};
    if address == 0 || address == source {
        return source;
    }
    let Some(block) = system.memory.phys.allocate(MemoryRegion::System, size) else {
        log::warn!("ldr:ro: no memory for a 0x{size:X}-byte module, running it from its buffer");
        return place(system, address, source, size);
    };
    let mut bytes = vec![0u8; size as usize];
    system.memory.read_bytes(source, &mut bytes);
    system.memory.write_physical(block.addr, &bytes);
    system.memory.map(address, block.addr, block.size, Permission::RW | Permission::EXECUTE, MemoryState::Code);
    system.cro.copies.insert(address, crate::cro::Copy { buffer: source, block, fixed: size });
    address
}

/// puts the module as ldr:ro left it into the title's buffer too,
/// the title copies the module's data (relocated) out of its buffer once LoadCRO returns,
/// and on the console both are the same memory by then
fn copy_back(system: &mut System, address: u32, size: u32) {
    let Some(copy) = system.cro.copies.get(&address).copied() else { return };
    let mut bytes = vec![0u8; size as usize];
    system.memory.read_bytes(address, &mut bytes);
    system.memory.write_bytes(copy.buffer, &bytes);
}

/// keeps what fixing left of a module and lets the rest of its memory go, the title maps the next module over it
fn shrink(system: &mut System, address: u32, fixed: u32) {
    use crate::memory::{MemoryState, Permission};
    let Some(copy) = system.cro.copies.get_mut(&address) else { return };
    copy.fixed = fixed;
    let kept = zakuro_common::bits::align_up(fixed, 0x1000);
    if kept >= copy.block.size {
        return;
    }
    let block = copy.block;
    let tail = crate::memory::physical::PhysicalBlock { addr: block.addr + kept, size: block.size - kept };
    copy.block.size = kept;
    system.memory.unmap(address, block.size);
    system.memory.map(address, block.addr, kept, Permission::RW | Permission::EXECUTE, MemoryState::Code);
    system.memory.phys.free(tail);
}

/// hands an unloaded module back to the title in its buffer (the part fixing left, as ldr:ro left it) and lets its memory go
fn copy_out(system: &mut System, address: u32) {
    let Some(copy) = system.cro.copies.remove(&address) else { return };
    let mut bytes = vec![0u8; copy.fixed.min(copy.block.size) as usize];
    system.memory.read_bytes(address, &mut bytes);
    system.memory.write_bytes(copy.buffer, &bytes);
    system.memory.unmap(address, copy.block.size);
    system.memory.phys.free(copy.block);
}

/// makes the module visible at the addr the title wants to run it from and returns that addr
fn place(system: &mut System, address: u32, source: u32, size: u32) -> u32 {
    if address == 0 || address == source {
        return source;
    }
    if system.memory.mirror(address, source, size) {
        address
    } else {
        log::warn!(
            "ldr:ro: could not mirror 0x{source:08X} at 0x{address:08X}, using the buffer"
        );
        source
    }
}
