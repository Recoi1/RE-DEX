use zakuro_cpu::{Bus, Cpu, Exit};
use zakuro_fs::{Ncsd, RomFs, ncch::NcchHeader};
use zakuro_gpu::{Renderer, SoftwareRenderer};

struct Ram(Vec<u8>);
impl Bus for Ram {
    fn read8(&mut self, a: u32) -> u8 { self.0[a as usize] }
    fn read16(&mut self, a: u32) -> u16 { u16::from_le_bytes(self.0[a as usize..a as usize + 2].try_into().unwrap()) }
    fn read32(&mut self, a: u32) -> u32 { u32::from_le_bytes(self.0[a as usize..a as usize + 4].try_into().unwrap()) }
    fn write8(&mut self, a: u32, v: u8) { self.0[a as usize] = v }
    fn write16(&mut self, a: u32, v: u16) { self.0[a as usize..a as usize + 2].copy_from_slice(&v.to_le_bytes()) }
    fn write32(&mut self, a: u32, v: u32) { self.0[a as usize..a as usize + 4].copy_from_slice(&v.to_le_bytes()) }
}

#[test]
fn cpu_runs_snippet() {
    // mov r0,#1; add r0,r0,#41; bx lr ; then svc #0 at lr=0x10
    let code = [0xE3A00001u32, 0xE2800029, 0xE12FFF1E, 0, 0xEF000000];
    let mut ram = Ram(vec![0; 0x100]);
    for (i, w) in code.iter().enumerate() { ram.write32(i as u32 * 4, *w); }
    let mut cpu = Cpu::new();
    cpu.regs[zakuro_cpu::PC] = 0;
    cpu.regs[zakuro_cpu::LR] = 0x10;
    let exit = cpu.run(&mut ram, 100);
    assert_eq!(cpu.regs[0], 42);
    assert!(matches!(exit, Exit::Supervisor(0)), "{exit:?}");
}

#[test]
fn fs_opens_cci() {
    let Ok(path) = std::env::var("XR_CCI") else {
        eprintln!("SKIP fs_opens_cci: XR_CCI not set");
        return;
    };
    let f = std::fs::File::open(&path).expect("open cci");
    let data = unsafe { memmap2::Mmap::map(&f) }.expect("mmap");
    let part = Ncsd::parse(&data).unwrap().executable_partition().unwrap();
    let ncch = NcchHeader::parse(&data[part.offset as usize..]).unwrap();
    assert_eq!(ncch.program_id, 0x0004000000055D00);
    let romfs = RomFs::parse(&data, part.offset + ncch.romfs_offset).unwrap();
    let (_dirs, files) = romfs.count_entries();
    assert!(files >= 600, "only {files} romfs files");
}

#[test]
fn gpu_builds() {
    let r = SoftwareRenderer::default();
    assert_eq!(r.name(), "software");
}
