//! xr-hle over a flat guest region: Pokémon X loads, an SVC dispatches

use xr_hle::{kernel, loader, memory, Config, System};
use zakuro_cpu::Bus;

/// the user's decrypted .cci (env XR_CCI), none skips; game data never enters the repo
fn cci() -> Option<String> {
    std::env::var("XR_CCI").ok().filter(|path| !path.is_empty())
}

#[test]
fn loads_x_into_flat_memory() {
    let Some(path) = cci() else {
        eprintln!("XR_CCI not set, skipping");
        return;
    };
    let mut system = loader::load(&path, Config::default(), memory::reserve()).expect("X loads");
    let title = system.title.as_ref().unwrap();
    let (code, _) = xr_hle::mods::code(title, None).unwrap();
    let app_bytes = title.exheader.system_mode.application_memory();

    let first = u32::from_le_bytes(code[..4].try_into().unwrap());
    assert_eq!(system.memory.read32(0x0010_0000), first);
    // the same word straight from the host, guest addr a is base + a
    let host = unsafe { (system.memory.base().add(0x0010_0000) as *const u32).read_unaligned() };
    assert_eq!(host, first);

    // config page APPMEMALLOC
    let appmemalloc = system.memory.read32(0x1FF8_0040);
    assert_ne!(appmemalloc, 0);
    assert_eq!(appmemalloc, app_bytes);

    let main = system.kernel.threads.iter().find(|thread| thread.name == "main").expect("a main thread");
    assert_eq!(main.entry, 0x0010_0000);
    assert_eq!(main.priority, 48);
    assert_eq!(main.tls, 0x1FF8_2000);
}

#[test]
fn svc_tick_via_dispatch() {
    let mut system = System::new(Config::default());
    system.cpu.cycles = 0x0000_0001_2345_6789;
    system.cpu.regs[0] = 0xDEAD_BEEF;
    kernel::svc::dispatch(&mut system, 0x28);
    assert_eq!((system.cpu.regs[0], system.cpu.regs[1]), (0x2345_6789, 1));
    assert!(system.cpu.cycles > 0x0000_0001_2345_6789);
}
