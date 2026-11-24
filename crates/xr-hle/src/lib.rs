//! the emulated console, cpu, memory, HLE kernel and services, GPU, and the loop driving them all

pub mod camera;
pub mod cheats;
pub mod enhancements;
pub mod cro;
pub mod download_play;
pub mod idle;
pub mod kernel;
pub mod loader;
pub mod memory;
pub mod microphone;
pub mod mods;
pub mod replay;
pub mod room;
pub mod services;
pub mod state;

use std::collections::{BTreeMap, BTreeSet};

use zakuro_common::memory_map::*;
use zakuro_common::ConsoleModel;
use zakuro_cpu::{Cpu, Exit};
use zakuro_fs::Title;
use zakuro_gpu::{Gpu, GpuMemory, Renderer, SoftwareRenderer};

use kernel::thread::{ThreadId, WaitResult, WaitSyscall, THREAD_EXIT_MAGIC};
use kernel::Kernel;
use memory::{Memory, MemoryState, Permission};
use services::hid::InputState;
use services::ServiceState;

/// console settings the frontend can change
#[derive(Debug, Clone)]
pub struct Config {
    pub model: ConsoleModel,
    pub new3ds: bool,
    /// 0 is japan, 1 usa, 2 europe
    pub region: u8,
    pub language: u8,
    pub slider_3d: f32,
    /// where saves and dumped system files live, the cwd when none
    pub data_dir: Option<std::path::PathBuf>,
    /// the saves folder (a character's own, see redex --character), `<data_dir>/user` when none
    pub user_dir: Option<std::path::PathBuf>,
    /// draw on the host GPU thru Vulkan when there's one that can
    pub hardware_renderer: bool,
    /// swap the title's textures for the texture pack in its mods folder, needs the host GPU
    pub texture_packs: bool,
    /// the title's update CIA, whose code and data it runs
    pub update: Option<std::path::PathBuf>,
    /// the title's DLC CIAs
    pub dlc: Vec<std::path::PathBuf>,
    /// enhancements turned on, by name, per title id
    pub enhancements: std::collections::BTreeMap<u64, Vec<String>>,
    /// resolution multiplier for the host GPU
    pub resolution: u32,
    /// the Vulkan presenter's device, to draw on and show the screens straight from when it can
    pub device: Option<std::sync::Arc<zakuro_gpu::SharedDevice>>,
    /// console clock start in millis since 1900, host time when none, a fixed one makes runs repeat exactly
    pub clock: Option<u64>,
    /// draw a picture between every two for titles drawing every other refresh (see System::pace)
    /// when the host GPU draws the 3D and shows the screens straight from its device
    pub interpolate: bool,
    /// the owner's name, up to 10 chars, titles show it and local wireless tells others
    pub username: String,
    /// the number telling this console apart over local wireless, config block 0x00090000
    pub console_id: u64,
    /// the game's Download Play child decrypted, a host sends it to guests, the card's copy is encrypted
    pub dlp_child: Option<std::path::PathBuf>,
    /// what a Download Play child runs w/, host addr and network passphrase, APT:GetWirelessRebootInfo's answer
    pub wireless_reboot: Option<[u8; 16]>,
    /// the interpreter skips to the next event past loops that change nothing, see idle
    pub skip_idle_loops: bool,
    /// leave `<data>/logs/loadcro.csv` alone (a preview run beside a game that is using it)
    pub no_cro_log: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            model: ConsoleModel::Old3ds,
            new3ds: false,
            region: services::cfg::REGION_USA,
            language: services::cfg::LANGUAGE_ENGLISH,
            slider_3d: 0.0,
            data_dir: None,
            user_dir: None,
            hardware_renderer: false,
            texture_packs: true,
            update: None,
            dlc: Vec::new(),
            enhancements: std::collections::BTreeMap::new(),
            resolution: 1,
            device: None,
            clock: None,
            interpolate: false,
            username: "Zakuro".into(),
            console_id: 0x5A4B_0000_0000_0001,
            dlp_child: None,
            wireless_reboot: None,
            skip_idle_loops: true,
            no_cro_log: false,
        }
    }
}

/// how a frame of emulation ended
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameOutcome {
    /// a full frame's cycles ran
    Completed,
    /// the title called svcExitProcess
    Exited,
    /// the title called svcBreak or hit an instr we don't have
    Faulted,
}

/// a screen's picture as RGBA plus width and height
pub type Screen = (std::sync::Arc<Vec<u8>>, u32, u32);

/// frame interpolation's refresh count, see System::pace
struct Pacing {
    refreshes: u64,
    /// refresh the top screen last got a new picture at, and that picture
    new_at: u64,
    last: Option<zakuro_gpu::ScreenRef>,
    /// how the draws of the frame b4 matched the one b4 that
    matches: Option<zakuro_gpu::Matches>,
    /// the pictures in between the screens' picture and the one b4, from the refresh it came at till the next
    between: Option<Motion>,
    /// what each screen (top, bottom, the right eye) showed when the top's picture came: a screen whose picture came
    /// later (a bottom screen at another pace) shows its own, not pictures in between of another
    with: [Option<zakuro_gpu::ScreenRef>; 3],
    /// pictures shown w/ pictures in between so far
    shown: u64,
    /// the monitor rate the pictures in between are made for, see steps_for
    hz: u32,
    /// the refresh the camera last cut elsewhere at, and whether a new picture came after it (see camera_cut)
    cut: Option<(u64, bool)>,
}

impl Default for Pacing {
    fn default() -> Self {
        Pacing { refreshes: 0, new_at: 0, last: None, matches: None, between: None, with: [None; 3], shown: 0, hz: 60, cut: None }
    }
}

/// refreshes after a camera cut w/o pictures in between (see System::camera_cut): X opens its door transition's iris
/// over the 16 after the cut, and in pictures in between its hole and the black around it don't agree (the hole's
/// corners clipped: one part follows the frame as is, the other moves in between; M3 strips, plan 6 Task 4)
const CUT_HOLD: u64 = 24;

/// whether a new picture at `refresh` goes w/o pictures in between for a camera cut (System::camera_cut: the cut's
/// refresh, whether a picture came after it), and the cut after it: the pictures from the cut's refresh for CUT_HOLD
/// refreshes, and the first one after the cut however late, go w/o
fn cut_holds(cut: Option<(u64, bool)>, refresh: u64) -> (bool, Option<(u64, bool)>) {
    let Some((at, came)) = cut else { return (false, None) };
    let held = !came || refresh <= at + CUT_HOLD;
    let came = came || refresh > at;
    (held, (!(came && refresh > at + CUT_HOLD)).then_some((at, came)))
}

/// a top picture w/ pictures in between it and the one b4 (frame interpolation, see System::pace): the refresh it came
/// at (it tells pictures apart), the refreshes it took to come (2 or 3), and K: picture k in between (from 0) is
/// (k + 1)/K of the way from the one b4 to it
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Motion {
    pub picture: u64,
    pub gap: u64,
    pub steps: usize,
}

/// K, the steps between two of a title's pictures `gap` refreshes apart for a monitor at `hz`: enough for a picture
/// each refresh of the monitor, 2 to 8 (one in between to seven); at 60 Hz, 2 (30 fps) or 3 (20 fps)
pub fn steps_for(hz: u32, gap: u64) -> usize {
    let refresh_hz = kernel::thread::CPU_CLOCK_HZ as f64 / CYCLES_PER_FRAME as f64;
    // a 59.94 or 60 Hz monitor is the console's rate, not a bit more
    let steps = (f64::from(hz) * gap as f64 / refresh_hz - 0.1).ceil();
    steps.clamp(2.0, 8.0) as usize
}

/// XR_FRAMESTATS: how often the top screen got a new picture, counted in refreshes since the one b4 (gaps[1] is every
/// refresh, gaps[2] every other, gaps[8] eight or more), and how the interpolation matched at each new picture
/// counts only while `on`, apart from `pace`, which only runs with interpolation
#[derive(Default, Clone)]
pub struct Cadence {
    pub on: bool,
    pub refreshes: u64,
    /// new top pictures so far, the id the frontend gives a picture
    pub pictures: u64,
    pub gaps: [u64; 9],
    /// the new pictures w/ matches (interpolation on), those that were smooth, and the totals of their draws
    pub sets: u64,
    pub smooth: u64,
    pub draws: u64,
    pub matched: u64,
    pub moved: u64,
    pub jumped: u64,
    pub jumping: u64,
    /// the bottom screen's gaps the same way, and how many of its new pictures came at a refresh the top got one too
    /// (Smooth motion only shows a bottom screen's pictures in between for those, see gpu_between)
    pub bottom_gaps: [u64; 9],
    pub bottom_with_top: u64,
    /// smooth new pictures shown w/o pictures in between all the same, for a camera cut (see System::camera_cut)
    pub held: u64,
    new_at: u64,
    last: Option<zakuro_gpu::ScreenRef>,
    bottom_new_at: u64,
    bottom_last: Option<zakuro_gpu::ScreenRef>,
}

impl Cadence {
    /// the counts so far on one line, for the log
    pub fn line(&self) -> String {
        format!(
            "refreshes {} pictures {} gaps {:?} sets {} smooth {} draws {} matched {} moved {} jumped {} jumping {} bottom gaps {:?} with the top {} held {}",
            self.refreshes, self.pictures, &self.gaps[1..], self.sets, self.smooth, self.draws, self.matched, self.moved, self.jumped, self.jumping, &self.bottom_gaps[1..], self.bottom_with_top, self.held
        )
    }
}

pub struct System {
    pub cpu: Cpu,
    pub memory: Memory,
    pub kernel: Kernel,
    pub services: ServiceState,
    pub gpu: Gpu,
    pub renderer: Box<dyn Renderer>,
    pub title: Option<Title>,
    /// the title's DLC
    pub dlc: Vec<zakuro_fs::Dlc>,
    /// the title's cheats, from its file in the data folder
    pub cheats: Vec<cheats::Cheat>,
    pub config: Config,

    pub exited: bool,
    pub broke: bool,
    /// everything the title wrote w/ svcOutputDebugString
    pub debug_output: String,
    pub unimplemented_svcs: BTreeSet<u32>,
    pub services_seen: BTreeSet<String>,
    pub lcd_force_black: bool,
    /// the scaled picture each screen had at the last present, it goes up at the next once the GPU had a frame to draw it
    showing: [Option<zakuro_gpu::ScreenRef>; 3],
    /// frame interpolation's refresh count
    pacing: Pacing,
    pub cadence: Cadence,
    /// the dynamic module loader's state
    pub cro: cro::CroManager,
    /// what the cameras see, from the frontend
    pub camera: Option<Box<dyn camera::Camera>>,
    /// what the mic hears, from the frontend
    pub microphone: Option<Box<dyn microphone::Microphone>>,
    /// the room local wireless goes thru, none w/ wireless off
    pub room: Option<room::Link>,
    /// errors the title reported thru err:f, newest last
    pub fatal_errors: Vec<String>,
    /// undefined instrs we've seen, keeps the log readable
    undefined_seen: BTreeMap<u32, u32>,
    /// branches into unmapped memory, keyed by target
    pub prefetch_aborts: BTreeMap<u32, u32>,
    /// the last few instr addrs so a fault can say how it got there
    history: [u32; HISTORY_LENGTH],
    history_index: usize,
    /// frames done since boot
    pub frames: u64,
    /// the console clock at boot, millis since 1900
    pub(crate) boot_clock: u64,
    /// cycle the next end of frame work is due at
    pub next_frame_boundary: u64,
    /// cycle the DSP finishes its next audio frame at
    pub next_audio_frame: u64,
    /// cycle the scheduler gets forced to run next
    next_preempt: u64,

    /// sampling profiler, how often each thread was caught at each PC
    pub profile: Option<BTreeMap<(ThreadId, u32), u64>>,
    /// steps since the profiler last looked
    profile_skip: u32,

    /// loops that change nothing, the interpreter skips them
    pub idle: idle::IdleLoops,
    /// instrs the interpreter ran
    pub interpreted_instructions: u64,
    /// xr-hle: a mod (code.bin, code.ips, code.bps) changed the game's code, which recompiled code doesn't include
    pub code_modded: bool,
    /// xr-hle: a mod changed the game: its RomFS (a texture pack aside), exheader or code (online play is refused)
    pub game_modded: bool,
    /// xr-hle: GX ProcessCommandList commands run so far
    pub command_lists: u64,
    /// xr-hle: every service the title sent a request to, in first-call order, each w/ (command id, count) in
    /// first-call order, for the runtime's progress report
    pub service_calls: Vec<(String, Vec<(u16, u64)>)>,
    /// xr-hle: told about every CRO ldr:ro places or unloads, for a host that runs module code natively
    pub module_hook: Option<cro::ModuleHook>,
    /// xr-hle: what draws the 3D, "software" or the Vulkan device as the loader enabled it (name, scale), for the
    /// runtime's checkpoint log and script headers
    pub renderer_name: String,
}

/// cycles per frame of the screens, they refresh at 268111856 / 4481136 so ~59.83 Hz not 60
/// a title keeping time by frames drifts ahead of its sound otherwise, rhythm game notes slide off the music over a song
pub const CYCLES_PER_FRAME: u64 = 4_481_136;

/// cycles per audio frame, 160 samples of exactly 8192 cycles each (the DSP's 32728 Hz is the cpu clock / 8192)
pub const CYCLES_PER_AUDIO_FRAME: u64 = 160 * 8192;
/// samples the DSP plays per second, one every 8192 cycles
pub const AUDIO_SAMPLE_RATE: f64 = kernel::thread::CPU_CLOCK_HZ as f64 / 8192.0;

/// how often the scheduler gets forced to run even if no thread yields
pub const PREEMPT_INTERVAL: u64 = 8192;

/// instr addrs kept for fault reports
const HISTORY_LENGTH: usize = 64;

/// what one [System::step_instruction] did
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stepped {
    Ran,
    /// an idle loop was skipped, the clock moved and a reschedule is due
    Idled,
    /// the current thread ended (returned to THREAD_EXIT_MAGIC, or branched to unmapped memory)
    ThreadEnded,
    /// svc #imm, not yet dispatched
    Svc(u32),
}

/// what one [System::step] call did
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    Ran,
    Exited,
    Faulted,
}

impl System {
    pub fn new(config: Config) -> System {
        let boot_clock = config.clock.unwrap_or_else(memory::config::host_clock);
        let idle = idle::IdleLoops::new(config.skip_idle_loops);
        let app_bytes = 64 * 1024 * 1024;
        let mut services = ServiceState::default();
        if let Some(dir) = config.user_dir.clone().or(config.data_dir.as_ref().map(|d| d.join("user"))) {
            services.fs.user_dir = dir;
        }
        System {
            cpu: Cpu::new(),
            memory: Memory::new(config.new3ds, app_bytes),
            kernel: Kernel::new(0, memory::MemoryRegion::Application, linear_heap_base(0)),
            services,
            gpu: Gpu::new(),
            renderer: Box::new(SoftwareRenderer::default()),
            title: None,
            dlc: Vec::new(),
            cheats: Vec::new(),
            config,
            exited: false,
            broke: false,
            debug_output: String::new(),
            unimplemented_svcs: BTreeSet::new(),
            services_seen: BTreeSet::new(),
            lcd_force_black: false,
            showing: [None, None, None],
            pacing: Pacing::default(),
            cadence: Cadence::default(),
            cro: cro::CroManager::default(),
            camera: None,
            microphone: None,
            room: None,
            fatal_errors: Vec::new(),
            undefined_seen: BTreeMap::new(),
            prefetch_aborts: BTreeMap::new(),
            history: [0; HISTORY_LENGTH],
            history_index: 0,
            frames: 0,
            boot_clock,
            next_frame_boundary: CYCLES_PER_FRAME,
            next_audio_frame: CYCLES_PER_AUDIO_FRAME,
            next_preempt: PREEMPT_INTERVAL,
            profile: None,
            profile_skip: 0,
            idle,
            interpreted_instructions: 0,
            code_modded: false,
            game_modded: false,
            command_lists: 0,
            service_calls: Vec::new(),
            module_hook: None,
            renderer_name: "software".into(),
        }
    }

    // process setup

    /// maps the page w/ a thread's TLS block if it isn't yet, and clears the block (an ended thread may have used it)
    pub fn map_tls_page(&mut self, thread: ThreadId) {
        let tls = self.kernel.thread(thread).tls;
        let page = tls & !PAGE_MASK;
        if self.memory.mapping_at(page).is_some() {
            self.memory.write_bytes(tls, &[0; TLS_ENTRY_SIZE as usize]);
            return;
        }
        let Some(block) = self
            .memory
            .phys
            .allocate(memory::MemoryRegion::Base, PAGE_SIZE)
        else {
            log::error!("out of memory allocating a TLS page");
            return;
        };
        self.memory.map(
            page,
            block.addr,
            PAGE_SIZE,
            Permission::RW,
            MemoryState::Locked,
        );
        // TLS must start zeroed, the IPC command buffer lives in it
        self.memory.zero_physical(block.addr, PAGE_SIZE);
    }

    // the run loop

    /// runs one frame of emulation
    pub fn run_frame(&mut self) -> FrameOutcome {
        // HID samples its inputs on its own, pressed or not
        let input = self.services.hid.input;
        self.idle_interrupted(false);
        services::hid::update(self, input);
        cheats::run(self);
        // run up to the boundary step() would end the frame at and end it here once,
        // not again when the next frame's first step sees it passed
        let deadline = self.next_frame_boundary;
        while self.cpu.cycles < deadline {
            match self.step(Some(deadline)) {
                StepOutcome::Ran => {}
                StepOutcome::Exited => return FrameOutcome::Exited,
                StepOutcome::Faulted => return FrameOutcome::Faulted,
            }
        }
        self.next_frame_boundary = self.cpu.cycles + CYCLES_PER_FRAME;
        self.end_frame();
        FrameOutcome::Completed
    }

    /// moves the machine one instr forward, doing any due scheduling first
    pub fn step(&mut self, deadline: Option<u64>) -> StepOutcome {
        if self.exited {
            return StepOutcome::Exited;
        }
        if self.broke {
            return StepOutcome::Faulted;
        }

        // fire the end of frame work on time even when a tool is stepping, else a title waiting on vblank never wakes
        if self.cpu.cycles >= self.next_frame_boundary {
            self.next_frame_boundary = self.cpu.cycles + CYCLES_PER_FRAME;
            self.end_frame();
        }
        if self.cpu.cycles >= self.next_audio_frame {
            self.next_audio_frame = self.cpu.cycles + CYCLES_PER_AUDIO_FRAME;
            self.audio_frame();
        }

        if self.kernel.reschedule_pending || self.kernel.current_thread.is_none() {
            self.reschedule();
        }

        if self.kernel.current_thread.is_none() {
            // everything's blocked
            let mut limit = deadline.unwrap_or(self.cpu.cycles + CYCLES_PER_FRAME);
            // an audio thread waits on the DSP's interrupt, which only fires at an audio frame boundary, skipping one would lose it
            if self.services.dsp.running {
                limit = limit.min(self.next_audio_frame);
            }
            match self.kernel.next_event() {
                Some(tick) if tick > self.cpu.cycles => self.cpu.cycles = tick.min(limit),
                _ => self.cpu.cycles = limit,
            }
            self.kernel.reschedule_pending = true;
            return StepOutcome::Ran;
        }

        match self.step_instruction(deadline) {
            Stepped::Svc(number) => kernel::svc::dispatch(self, number),
            Stepped::Ran | Stepped::Idled | Stepped::ThreadEnded => {}
        }

        self.preempt();

        if self.exited {
            StepOutcome::Exited
        } else if self.broke {
            StepOutcome::Faulted
        } else {
            StepOutcome::Ran
        }
    }

    /// one guest instr on the current thread w/o any scheduling, the part of [System::step] an executor that
    /// schedules on its own (xr-runtime) shares: thread exit at THREAD_EXIT_MAGIC, prefetch aborts, idle loop skips,
    /// undefined/bkpt skipping and halts; an SVC is handed back for the caller to dispatch
    pub fn step_instruction(&mut self, deadline: Option<u64>) -> Stepped {
        // a thread whose entry returned branches here
        if self.cpu.regs[15] == THREAD_EXIT_MAGIC {
            if let Some(id) = self.kernel.current_thread {
                self.kernel.end_thread(id);
            }
            return Stepped::ThreadEnded;
        }

        // a branch thru an uninitialized func ptr lands on the null page, whose zero words decode as harmless no ops
        let pc = self.cpu.regs[15];
        if !self.memory.is_executable(pc) {
            let count = self.prefetch_aborts.entry(pc).or_insert(0);
            *count += 1;
            if *count == 1 {
                let thread = self
                    .kernel
                    .current()
                    .map_or("?".to_owned(), |t| t.name.clone());
                log::error!(
                    "prefetch abort: {thread} branched to unmapped 0x{pc:08X} from 0x{:08X}",
                    self.cpu.regs[14]
                );
                log::error!("{:?}", self.cpu);
                log::error!("the instructions leading here were:");
                let recent = self.recent_instructions();
                for &entry in recent.iter().rev().take(8).rev() {
                    let address = entry & !1;
                    let mut bytes = [0u8; 8];
                    self.memory.read_bytes(address & !3, &mut bytes);
                    log::error!(
                        "  0x{address:08X} {} [{:02X?}]",
                        if entry & 1 != 0 { "T" } else { "A" },
                        bytes
                    );
                }
                self.fatal_errors.push(format!(
                    "branch to unmapped 0x{pc:08X} (lr 0x{:08X})",
                    self.cpu.regs[14]
                ));
            }
            // stop the thread not the machine, the rest of the title may still get somewhere, the report says what happened
            if let Some(id) = self.kernel.current_thread {
                self.kernel.end_thread(id);
            }
            return Stepped::ThreadEnded;
        }

        self.note(pc);

        // xr-hle: the AOT branch (run_recompiled) is cut, native code runs through xr-runtime's executor
        let exit = {
            // a thread polling in a loop that changes nothing waits for the next event, like on a console spinning freely
            if let Some(until) = self.idle.due(self.cpu.cycles).then(|| self.idle_until(deadline)).flatten() {
                self.cpu.cycles = until;
                self.kernel.reschedule_pending = true;
                return Stepped::Idled;
            }
            self.interpreted_instructions += 1;
            self.cpu.step(&mut self.memory)
        };
        if exit.is_some() {
            self.idle_interrupted(true);
        }
        match exit {
            None => {}
            Some(Exit::Supervisor(number)) => return Stepped::Svc(number),
            Some(Exit::Undefined { pc, opcode }) => {
                let count = self.undefined_seen.entry(pc).or_insert(0);
                *count += 1;
                if *count == 1 {
                    log::error!("undefined instruction 0x{opcode:08X} at 0x{pc:08X}");
                    log::error!("{:?}", self.cpu);
                }
                // skip it and keep going, one bad decode shouldn't end the session while the cpu is still being filled in
                let step = if self.cpu.cpsr.thumb { 2 } else { 4 };
                self.cpu.regs[15] = pc.wrapping_add(step);
            }
            Some(Exit::Breakpoint { pc, imm }) => {
                log::warn!("bkpt #{imm} at 0x{pc:08X}");
                let step = if self.cpu.cpsr.thumb { 2 } else { 4 };
                self.cpu.regs[15] = pc.wrapping_add(step);
            }
            Some(Exit::Halted) => {
                self.cpu.resume();
                self.kernel.reschedule_pending = true;
            }
            Some(Exit::Timeout) => {}
        }
        Stepped::Ran
    }

    /// the scheduling pass a step makes first when due
    pub fn reschedule(&mut self) {
        self.kernel.reschedule_pending = false;
        let tick = self.cpu.cycles;
        if self.kernel.schedule(&mut self.cpu, tick) {
            self.idle_interrupted(true);
        }
        // a thread released here that keeps the core gets its result too b4 it runs on, not over whatever it does next
        self.apply_wait_result();
    }

    /// notes where a step runs the guest from, for fault reports and the profiler
    fn note(&mut self, pc: u32) {
        self.history[self.history_index] = pc | self.cpu.cpsr.thumb as u32;
        self.history_index = (self.history_index + 1) % HISTORY_LENGTH;
        if self.profile.is_some() {
            self.sample();
        }
    }

    /// gives the scheduler a regular chance, so a thread that never syscalls can't hog the core
    pub fn preempt(&mut self) {
        if self.cpu.cycles >= self.next_preempt {
            self.next_preempt = self.cpu.cycles + PREEMPT_INTERVAL;
            self.kernel.reschedule_pending = true;
        }
    }

    /// GPU coherency phase 1: writes back everything the host GPU drew that guest memory doesn't have yet
    /// runs after every command list and display transfer (so the cpu's later writes land on top), and at end_frame as a backstop
    /// TODO(phase 2): per-access interception (VirtualProtect + the runtime's exception handler) instead of eagerly
    pub fn flush_gpu_guards(&mut self) {
        let linear_base = self.kernel.linear_base;
        for (addr, end) in self.memory.take_guards() {
            self.gpu.sync_memory(&mut GuestMemory { memory: &mut self.memory, linear_base, writes: 0 }, addr, end - addr);
        }
    }

    /// the most recently run instr addrs, oldest first
    pub fn recent_instructions(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity(HISTORY_LENGTH);
        for offset in 0..HISTORY_LENGTH {
            let entry = self.history[(self.history_index + offset) % HISTORY_LENGTH];
            if entry != 0 {
                out.push(entry);
            }
        }
        out
    }

    /// starts collecting PC samples
    pub fn enable_profiler(&mut self) {
        self.profile = Some(BTreeMap::new());
    }

    /// the hottest sampled spots, most frequent first
    pub fn hot_spots(&self, count: usize) -> Vec<(String, u32, u64)> {
        let Some(profile) = &self.profile else {
            return Vec::new();
        };
        let mut entries: Vec<(String, u32, u64)> =
            profile.iter().map(|(&(thread, pc), &hits)| (self.thread_name(thread), pc, hits)).collect();
        entries.sort_by_key(|&(_, _, hits)| std::cmp::Reverse(hits));
        entries.truncate(count);
        entries
    }

    /// how many samples caught each thread running, most first
    pub fn profiled_threads(&self) -> Vec<(String, u64)> {
        let Some(profile) = &self.profile else {
            return Vec::new();
        };
        let mut threads: BTreeMap<ThreadId, u64> = BTreeMap::new();
        for (&(thread, _), &hits) in profile {
            *threads.entry(thread).or_insert(0) += hits;
        }
        let mut threads: Vec<(String, u64)> = threads.into_iter().map(|(thread, hits)| (self.thread_name(thread), hits)).collect();
        threads.sort_by_key(|&(_, hits)| std::cmp::Reverse(hits));
        threads
    }

    fn thread_name(&self, id: ThreadId) -> String {
        self.kernel.threads.get(id as usize).map_or_else(|| format!("thread {id}"), |thread| thread.name.clone())
    }

    fn sample(&mut self) {
        // one step in so many, a prime so it doesn't beat in sync w/ a loop, keeps the profiler from slowing the run
        self.profile_skip = self.profile_skip.wrapping_add(1);
        if !self.profile_skip.is_multiple_of(61) {
            return;
        }
        let Some(id) = self.kernel.current_thread else {
            return;
        };
        let pc = self.cpu.regs[15];
        if let Some(profile) = &mut self.profile {
            *profile.entry((id, pc)).or_insert(0) += 1;
        }
    }

    /// writes back the result regs of a syscall that had blocked
    pub fn apply_wait_result(&mut self) {
        let Some(id) = self.kernel.current_thread else {
            return;
        };
        let thread = self.kernel.thread_mut(id);
        let (Some(result), Some(syscall)) = (thread.wait_result.take(), thread.wait_syscall.take())
        else {
            return;
        };

        match (syscall, result) {
            (WaitSyscall::WaitSynchronization1, WaitResult::Signaled(_)) => {
                self.cpu.regs[0] = 0;
            }
            (WaitSyscall::WaitSynchronizationN, WaitResult::Signaled(index)) => {
                self.cpu.regs[0] = 0;
                self.cpu.regs[1] = index as u32;
            }
            (WaitSyscall::ArbitrateAddress, WaitResult::Signaled(_)) => {
                self.cpu.regs[0] = 0;
            }
            (_, WaitResult::TimedOut) => {
                self.cpu.regs[0] = zakuro_common::result::errors::TIMEOUT.0;
            }
            (WaitSyscall::SleepThread, _) => {
                self.cpu.regs[0] = 0;
            }
        }
    }

    /// everything between frames, vblank, input, clock
    pub fn end_frame(&mut self) {
        self.idle_interrupted(false);
        self.frames += 1;
        // GPU coherency phase 1 backstop, submit_command_list and display_transfer already wrote back what they drew
        self.flush_gpu_guards();

        // refresh the kernel's shared page so the guest's clock moves
        let tick = self.cpu.cycles;
        memory::config::update_datetime(self.memory.shared_page_mut(), self.boot_clock, tick);

        // regather the timers minus the ones closed since, they fire when the scheduler reaches their tick
        self.kernel.timers.clear();
        for (id, object) in self.kernel.objects.iter() {
            if let kernel::object::KObject::Timer(_) = object {
                self.kernel.timers.push(id);
            }
        }

        self.renderer.end_frame();

        // grab any buffer swap the game queued straight in GSP shared memory b4 the LCDs latch whatever's configured
        services::gsp::refresh(self);
        if self.gpu.interpolates() {
            self.pace();
        }
        if self.cadence.on {
            self.note_cadence();
        }

        // both LCDs finish scanning out, in that order
        services::gsp::signal_interrupt(self, services::gsp::InterruptId::Pdc0);
        services::gsp::signal_interrupt(self, services::gsp::InterruptId::Pdc1);

    }

    /// XR_FRAMESTATS: counts the refreshes since the top screen's picture last changed, as pace does (see Cadence)
    fn note_cadence(&mut self) {
        self.cadence.refreshes += 1;
        let top = self.screen_ref(zakuro_common::Screen::Top, false);
        let bottom = self.screen_ref(zakuro_common::Screen::Bottom, false);
        let cadence = &mut self.cadence;
        let top_new = top.is_some() && top != cadence.last;
        if bottom.is_some() && bottom != cadence.bottom_last {
            if cadence.bottom_last.is_some() {
                cadence.bottom_gaps[(cadence.refreshes - cadence.bottom_new_at).min(8) as usize] += 1;
                cadence.bottom_with_top += top_new as u64;
            }
            cadence.bottom_new_at = cadence.refreshes;
            cadence.bottom_last = bottom;
        }
        if !top_new {
            return;
        }
        let cadence = &mut self.cadence;
        let gap = cadence.refreshes - cadence.new_at;
        cadence.new_at = cadence.refreshes;
        // the first picture has nothing b4 it to be a gap from
        if cadence.last.is_some() {
            cadence.gaps[gap.min(8) as usize] += 1;
        }
        cadence.last = top;
        cadence.pictures += 1;
        if let Some(m) = self.pacing.matches.filter(|_| self.gpu.interpolates()) {
            let cadence = &mut self.cadence;
            cadence.sets += 1;
            cadence.smooth += m.smooth() as u64;
            cadence.draws += m.draws as u64;
            cadence.matched += m.matched as u64;
            cadence.moved += m.moved as u64;
            cadence.jumped += m.jumped as u64;
            cadence.jumping += m.jumping as u64;
        }
    }

    /// frame interpolation at a refresh
    /// while a title sets a new picture every other or third refresh, the GPU draws K-1 in between it and the one b4
    /// (see zakuro_gpu's interpolation, K from steps_for), from the previous frame's draws and this one's
    /// they stay the screens' (motion) from the refresh the title set its picture till its next; the window shows each
    /// in turn as its time comes, the title's own last (K-1)/K of a frame late
    /// a title at full rate or slower than every third refresh shows its own right away and nothing gets drawn in between
    fn pace(&mut self) {
        let pacing = &mut self.pacing;
        pacing.refreshes += 1;
        let top = self.screen_ref(zakuro_common::Screen::Top, false);
        let pacing = &mut self.pacing;
        if top.is_none() || top == pacing.last {
            if pacing.refreshes - pacing.new_at > 3 {
                // the title slowed down or stopped, its next frame's in between pictures would show too late
                self.gpu.set_twins(0);
            }
            return;
        }
        let gap = pacing.refreshes - pacing.new_at;
        pacing.new_at = pacing.refreshes;
        pacing.last = top;
        // the frame just drawn has its twins at the steps set when it began
        let steps = self.gpu.steps();
        let matches = self.gpu.frame_done();
        let with = [self.screen_ref(zakuro_common::Screen::Top, false), self.screen_ref(zakuro_common::Screen::Bottom, false), self.screen_ref(zakuro_common::Screen::Top, true)];
        let pacing = &mut self.pacing;
        pacing.matches = matches;
        let paced = (2..=3).contains(&gap);
        let cut;
        (cut, pacing.cut) = cut_holds(pacing.cut, pacing.refreshes);
        let matched = paced && steps >= 2 && matches.is_some_and(|matches| matches.smooth());
        if cut && matched {
            log::debug!("frame interpolation: frame {}'s picture shown w/o pictures in between, the camera cut (its draws matched)", self.frames);
            self.cadence.held += self.cadence.on as u64;
        }
        let smooth = matched && !cut;
        pacing.between = smooth.then_some(Motion { picture: pacing.refreshes, gap, steps });
        pacing.with = with;
        pacing.shown += smooth as u64;
        // the next frame gets pictures in between too while the title sticks to every other or third refresh
        let twins = if paced { steps_for(pacing.hz, gap) - 1 } else { 0 };
        self.gpu.set_twins(twins);
    }

    /// draws pictures in between each two of the title's from now on (when it draws every other or third refresh, see
    /// pace), or stops; says whether it does, needs the host GPU showing the screens
    pub fn set_interpolation(&mut self, on: bool) -> bool {
        self.pacing = Pacing { hz: self.pacing.hz, ..Pacing::default() };
        self.gpu.set_interpolation(on)
    }

    /// the monitor rate the pictures in between are made for from the next frame on (60: one between two pictures at
    /// 30 fps; 240: seven), see steps_for
    pub fn set_motion_rate(&mut self, hz: u32) {
        self.pacing.hz = hz;
    }

    /// the title's camera cut elsewhere this refresh (a hint a title's own reader of its camera gives, e.g. X's field
    /// camera; the draws' matches miss a cut where most draws keep their uniforms), told b4 end_frame: the new pictures
    /// from this refresh for CUT_HOLD refreshes, and the first one after it however late, get no pictures in between.
    /// This refresh's too bc which picture first shows the new camera depends on whether the title moved it b4 or
    /// after drawing this refresh's (X: after, the next one)
    pub fn camera_cut(&mut self) {
        // pace counts this refresh when end_frame runs it
        let at = self.pacing.refreshes + 1;
        self.pacing.cut = Some((at, false));
        log::info!("frame interpolation: the camera cut at frame {}, no pictures in between for {CUT_HOLD} refreshes", self.frames + 1);
    }

    /// how the last frame's draws matched the one b4, for frame interpolation
    pub fn interpolation_matches(&self) -> Option<zakuro_gpu::Matches> {
        self.pacing.matches
    }

    /// the pictures in between the screens' picture and the one b4, while it's the screens' (see pace)
    pub fn motion(&self) -> Option<Motion> {
        self.pacing.between.filter(|_| self.gpu.interpolates())
    }

    /// picture k (from 0) in between a screen's picture and the one b4 on the host GPU, like gpu_screen gives the
    /// screen's own; none when it has none: no motion, a picture of its own that came after the top's, no twin k
    pub fn gpu_between(&mut self, screen: zakuro_common::Screen, right: bool, k: usize) -> Option<zakuro_gpu::GpuScreen> {
        self.motion()?;
        if right && !self.draws_in_3d() {
            return None;
        }
        let now = self.screen_ref(screen, right)?;
        let i = match screen {
            _ if right => 2,
            zakuro_common::Screen::Top => 0,
            zakuro_common::Screen::Bottom => 1,
        };
        if self.pacing.with[i] != Some(now) {
            return None;
        }
        self.gpu.between_image(now, k)
    }

    /// the DSP finishing an audio frame, plays its voices one frame on and interrupts the title, whose sound lib updates once per interrupt
    pub fn audio_frame(&mut self) {
        self.idle_interrupted(false);
        // the whole opening froze till this ran every 4.9 ms, goddamn audio timing
        if self.services.dsp.running {
            services::dsp::advance(self);
            services::dsp::signal_semaphore(self);
        }
        // devices reporting at a title set rate, this is fine enough for them
        services::ir_rst::tick(self);
        services::ir::tick(self);
        services::cam::tick(self);
        services::mic::tick(self);
        services::uds::tick(self);
        services::dlp::tick(self);
    }

    /// stereo samples played since the last call, at AUDIO_SAMPLE_RATE
    pub fn take_audio(&mut self) -> Vec<[i16; 2]> {
        std::mem::take(&mut self.services.dsp.output)
    }

    /// what the software keyboard asks for while a title waits on it
    pub fn keyboard_request(&self) -> Option<&services::keyboard::Request> {
        services::keyboard::request(self)
    }

    /// closes the software keyboard w/ the typed text and the pressed button, 0 is the leftmost and the last one confirms
    pub fn answer_keyboard(&mut self, text: &str, button: usize) {
        services::keyboard::answer(self, text, button);
    }

    /// what the buttons, circle pad and touch screen are doing, HID reports it from the next frame on
    pub fn set_input(&mut self, input: InputState) {
        self.services.hid.input = input;
    }

    // GPU plumbing

    pub fn gpu_read_register(&mut self, offset: u32) -> u32 {
        self.gpu.read_external(offset)
    }

    pub fn gpu_write_register(&mut self, offset: u32, value: u32) {
        self.gpu.write_external(offset, value);
    }

    pub fn set_framebuffer(
        &mut self,
        screen: u32,
        active: u32,
        left: u32,
        right: u32,
        stride: u32,
        format: u32,
    ) {
        self.gpu
            .set_framebuffer(screen, active, left, right, stride, format);
    }

    pub fn submit_command_list(&mut self, paddr: u32, size: u32) {
        let linear_base = self.kernel.linear_base;
        let mut guest = GuestMemory {
            writes: 0,
            linear_base,
            memory: &mut self.memory,
        };
        self.gpu
            .process_command_list(&mut guest, self.renderer.as_mut(), paddr, size);
        // what it drew reaches memory now, b4 the cpu or a service writes there (a reused buffer, a texture upload)
        self.flush_gpu_guards();
    }

    /// makes guest memory hold what the host GPU drew over a range, b4 smth other than a draw touches it
    pub fn sync_gpu(&mut self, addr: u32, len: u32) {
        let linear_base = self.kernel.linear_base;
        let mut guest = GuestMemory {
            writes: 0,
            linear_base,
            memory: &mut self.memory,
        };
        self.gpu.sync_memory(&mut guest, addr, len);
    }

    /// writes what a service hands the title (a file it read, say) over guest memory, after what the host GPU drew there
    /// on the console the drawing hit memory long b4, and the title can put the buffer where one it finished drawing to was
    pub fn write_from_service(&mut self, addr: u32, data: &[u8]) {
        if !data.is_empty() {
            self.sync_gpu(addr, data.len() as u32);
        }
        self.memory.write_bytes(addr, data);
    }

    pub fn memory_fill(&mut self, start: u32, end: u32, value: u32, width: u32) {
        let linear_base = self.kernel.linear_base;
        let mut guest = GuestMemory {
            writes: 0,
            linear_base,
            memory: &mut self.memory,
        };
        self.gpu.memory_fill(&mut guest, start, end, value, width);
    }

    pub fn display_transfer(
        &mut self,
        input: u32,
        output: u32,
        input_dimensions: u32,
        output_dimensions: u32,
        flags: u32,
    ) {
        let linear_base = self.kernel.linear_base;
        let mut guest = GuestMemory {
            writes: 0,
            linear_base,
            memory: &mut self.memory,
        };
        self.gpu.display_transfer(
            &mut guest,
            input,
            output,
            input_dimensions,
            output_dimensions,
            flags,
        );
        // a transfer the GPU did lands in memory now too, see submit_command_list
        self.flush_gpu_guards();
    }

    pub fn texture_copy(
        &mut self,
        input: u32,
        output: u32,
        size: u32,
        input_gap: u32,
        output_gap: u32,
    ) {
        let linear_base = self.kernel.linear_base;
        let mut guest = GuestMemory {
            writes: 0,
            linear_base,
            memory: &mut self.memory,
        };
        self.gpu
            .texture_copy(&mut guest, input, output, size, input_gap, output_gap);
    }

    /// the nine words gsp::ImportDisplayCaptureInfo returns, virtual addrs as the title gave them
    pub fn display_capture_info(&mut self) -> [u32; 9] {
        let top = self.gpu.framebuffers[0];
        let bottom = self.gpu.framebuffers[1];
        let address = |paddr: u32| if paddr == 0 { 0 } else { services::gsp::physical_to_virtual(self, paddr) };
        [
            address(top.address_left()),
            address(top.address_right()),
            top.format,
            top.stride,
            address(bottom.address_left()),
            address(bottom.address_right()),
            bottom.format,
            bottom.stride,
            0,
        ]
    }

    /// a screen as RGBA at the host GPU's resolution, plus width and height
    /// a picture the GPU didn't draw (or the cpu changed since) is the console's own scaled up, so the size stays the same every frame
    pub fn read_screen_scaled(&mut self, screen: zakuro_common::Screen) -> Screen {
        self.read_scaled(screen, false)
    }

    /// the top screen's right eye picture, like read_screen_scaled gives the left, when the title draws in 3D
    pub fn read_right_eye_scaled(&mut self) -> Option<Screen> {
        self.draws_in_3d().then(|| self.read_scaled(zakuro_common::Screen::Top, true))
    }

    fn read_scaled(&mut self, screen: zakuro_common::Screen, right: bool) -> Screen {
        let (width, height) = (screen.width(), screen.height());
        let scale = self.gpu.scale();
        if let Some(scaled) = self.scaled_screen(screen, right) {
            return scaled;
        }
        let native = self.read_screen_at(screen, right);
        if scale == 1 {
            return (std::sync::Arc::new(native), width, height);
        }
        (std::sync::Arc::new(grow(&native, width, scale)), width * scale, height * scale)
    }

    /// does the title draw the top screen in 3D, a picture per eye
    /// its format asks for the screen's two buffers (the 2nd for the right eye),
    /// the console's GSP only turns 3D on w/ bit 5 set and bit 6 clear
    pub fn draws_in_3d(&self) -> bool {
        let config = self.gpu.framebuffers[0];
        config.format & 0x60 == 0x20 && config.address_right() != 0
    }

    /// moves the 3D slider, titles read it every frame to decide if and how deep to draw 3D, 0 down and 1 all the way up
    pub fn set_slider_3d(&mut self, value: f32) {
        self.config.slider_3d = value;
        memory::config::set_slider_3d(self.memory.shared_page_mut(), value);
    }

    /// where a screen's picture lives on the host GPU, for a presenter sharing its device to draw straight from
    /// none when the GPU doesn't show the screens itself or didn't draw what the screen shows (read_screen_scaled has that)
    /// the presenter's work runs after the GPU draws the picture, so the newest goes up
    pub fn gpu_screen(&mut self, screen: zakuro_common::Screen) -> Option<zakuro_gpu::GpuScreen> {
        self.gpu_picture(screen, false)
    }

    /// the top screen's right eye picture on the host GPU, like gpu_screen gives the left, when the title draws in 3D
    pub fn gpu_right_eye(&mut self) -> Option<zakuro_gpu::GpuScreen> {
        if !self.draws_in_3d() {
            return None;
        }
        self.gpu_picture(zakuro_common::Screen::Top, true)
    }

    fn gpu_picture(&mut self, screen: zakuro_common::Screen, right: bool) -> Option<zakuro_gpu::GpuScreen> {
        if !self.gpu.shows_directly() {
            return None;
        }
        let now = self.screen_ref(screen, right)?;
        self.gpu.screen_image(now)
    }

    /// a screen's picture as the GPU showing the screens has it, or picture k in between it and the previous (see pace),
    /// for saving; waits for the GPU
    pub fn upright_screen(&mut self, screen: zakuro_common::Screen, between: Option<usize>) -> Option<Screen> {
        let now = self.screen_ref(screen, false)?;
        let (image, scale) = self.gpu.upright_picture(now, between)?;
        Some((image, screen.width() * scale, screen.height() * scale))
    }

    /// the scaled picture the host GPU drew for a screen, when it's still what the screen shows
    fn scaled_screen(&mut self, screen: zakuro_common::Screen, right: bool) -> Option<Screen> {
        let (width, height) = (screen.width(), screen.height());
        let index = match screen {
            _ if right => 2,
            zakuro_common::Screen::Top => 0,
            zakuro_common::Screen::Bottom => 1,
        };
        let Some(now) = self.screen_ref(screen, right) else {
            self.showing[index] = None;
            return None;
        };
        // what the screen had a present ago goes up now, always that one, so each picture stays up as long as the title kept it
        let show = self.showing[index].replace(now).unwrap_or(now);
        let (image, scale) = self.gpu.scaled_picture(show).or_else(|| self.gpu.scaled_picture(now))?;
        Some((image, width * scale, height * scale))
    }

    /// the picture the host GPU drew for a screen when it's still what's shown, the top one's right eye when right
    fn screen_ref(&mut self, screen: zakuro_common::Screen, right: bool) -> Option<zakuro_gpu::ScreenRef> {
        let (width, height) = (screen.width(), screen.height());
        let index = match screen {
            zakuro_common::Screen::Top => 0,
            zakuro_common::Screen::Bottom => 1,
        };
        let config = self.gpu.framebuffers[index];
        let address = if right { config.address_right() } else { config.address_left() };
        let format = config.color_format();
        let bpp = format.bytes_per_pixel() as u32;
        // plain buffers go a row per screen column like display transfers leave them, rows can be longer than the screen
        let stride = if config.stride == 0 { height * bpp } else { config.stride };
        if self.lcd_force_black || address == 0 || !stride.is_multiple_of(bpp) || stride < height * bpp {
            return None;
        }
        // no sync first, that'd wait for the GPU to finish the picture
        // compared in place, copying both screens every frame costs more than comparing
        let base = services::gsp::physical_to_virtual(self, address);
        let len = width * stride;
        let mut guest = GuestMemory { linear_base: self.kernel.linear_base, memory: &mut self.memory, writes: 0 };
        if let Some(bytes) = guest.slice(base, len as usize) {
            return self.gpu.scaled_screen(base, (height, width), stride / bpp, format, bytes);
        }
        let mut bytes = vec![0u8; len as usize];
        self.memory.read_bytes(base, &mut bytes);
        self.gpu.scaled_screen(base, (height, width), stride / bpp, format, &bytes)
    }

    /// The creator's studio view (zakuro_gpu::studio): the player's draws of the last frame, again from an orbit camera.
    pub fn studio_frame(&mut self, yaw: f32, zoom: f32) -> Result<zakuro_gpu::studio::Frame, &'static str> {
        let linear_base = self.kernel.linear_base;
        self.gpu.studio_render(&mut GuestMemory { memory: &mut self.memory, linear_base, writes: 0 }, yaw, zoom)
    }

    /// reads one screen into a plain RGBA8 buffer for presenting
    pub fn read_screen(&mut self, screen: zakuro_common::Screen) -> Vec<u8> {
        self.read_screen_at(screen, false)
    }

    /// same, the top screen's right eye picture when right
    fn read_screen_at(&mut self, screen: zakuro_common::Screen, right: bool) -> Vec<u8> {
        let index = match screen {
            zakuro_common::Screen::Top => 0,
            zakuro_common::Screen::Bottom => 1,
        };
        let config = self.gpu.framebuffers[index];
        let width = screen.width();
        let height = screen.height();
        let mut out = vec![0u8; (width * height * 4) as usize];

        let address = if right { config.address_right() } else { config.address_left() };
        if self.lcd_force_black || address == 0 {
            return out;
        }

        let format = config.color_format();
        let bpp = format.bytes_per_pixel();
        let stride = if config.stride == 0 {
            height * bpp as u32
        } else {
            config.stride
        };

        let base = services::gsp::physical_to_virtual(self, address);
        let source_len = (stride * width) as usize;
        self.sync_gpu(base, source_len as u32);
        let mut source = vec![0u8; source_len];
        self.memory.read_bytes(base, &mut source);

        for x in 0..width {
            for y in 0..height {
                // column major in memory and the panel scans bottom to top, bc of course it does
                let offset = (x * stride + (height - 1 - y) * bpp as u32) as usize;
                if offset + bpp > source.len() {
                    continue;
                }
                let pixel = format.decode(&source[offset..offset + bpp]);
                let dst = ((y * width + x) * 4) as usize;
                out[dst..dst + 4].copy_from_slice(&pixel);
            }
        }
        out
    }

    /// a one line summary for the window title and the log
    pub fn status_line(&self) -> String {
        let mut line = format!(
            "frame {} | {} threads | {} modules | {} draws | {} transfers | {} fills | gpu {:.1?}",
            self.frames,
            self.kernel.live_thread_count(),
            self.cro.len(),
            self.gpu.draw_calls,
            self.gpu.transfers,
            self.gpu.fills,
            self.gpu.busy,
        );
        if self.gpu.interpolates() {
            line += &format!(" | in between {}", self.pacing.shown);
        }
        line
    }
}

/// lets the GPU reach guest memory by physical addr
struct GuestMemory<'a> {
    memory: &'a mut Memory,
    linear_base: u32,
    /// writes thru it so far, see GpuMemory::writes
    writes: u64,
}

impl GuestMemory<'_> {
    /// the bytes at addr in host memory, when they sit together
    fn host(&mut self, addr: u32, len: usize) -> Option<&mut [u8]> {
        // xr-hle: guest memory is flat, the addr translate made is the host offset
        self.memory.slice_mut(addr, u32::try_from(len).ok()?)
    }
}

impl GpuMemory for GuestMemory<'_> {
    fn read(&mut self, addr: u32, out: &mut [u8]) {
        self.memory.read_bytes(addr, out);
    }

    fn write(&mut self, addr: u32, data: &[u8]) {
        self.writes += 1;
        self.memory.write_bytes(addr, data);
    }

    fn read_u32(&mut self, addr: u32) -> u32 {
        // not the way the cpu reads, that could make the GPU write back from inside itself
        self.memory.peek32(addr)
    }

    fn guard(&mut self, addr: u32, len: u32) {
        self.memory.guard_cpu_reads(addr, len);
    }

    fn guard_writes(&mut self, addr: u32, len: u32) {
        self.memory.guard_cpu_writes(addr, len);
    }

    fn slice(&mut self, addr: u32, len: usize) -> Option<&[u8]> {
        self.host(addr, len).map(|slice| &*slice)
    }

    fn slice_mut(&mut self, addr: u32, len: usize) -> Option<&mut [u8]> {
        self.writes += 1;
        self.host(addr, len)
    }

    fn writes(&self) -> Option<u64> {
        Some(self.writes)
    }

    // xr-hle: no watch/unchanged, native code writes guest memory unseen, so the GPU rehashes (zakuro-gpu's fallback)

    fn translate(&self, paddr: u32) -> u32 {
        if paddr >= FCRAM_PADDR {
            self.linear_base + (paddr - FCRAM_PADDR)
        } else if (VRAM_PADDR..VRAM_PADDR + VRAM_SIZE).contains(&paddr) {
            VRAM_VADDR + (paddr - VRAM_PADDR)
        } else {
            paddr
        }
    }
}

/// an RGBA picture of some width scaled up by a whole factor, each pixel repeated
fn grow(pixels: &[u8], width: u32, scale: u32) -> Vec<u8> {
    let (width, scale) = (width as usize, scale as usize);
    let mut out = Vec::with_capacity(pixels.len() * scale * scale);
    for row in pixels.chunks_exact(width * 4) {
        let mut grown = Vec::with_capacity(row.len() * scale);
        for pixel in row.as_chunks::<4>().0 {
            for _ in 0..scale {
                grown.extend_from_slice(pixel);
            }
        }
        for _ in 0..scale {
            out.extend_from_slice(&grown);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernel::object::KObject;
    use kernel::thread::ThreadStatus;
    use zakuro_cpu::Bus;

    /// a camera cut holds off pictures in between: its refresh's picture, the next CUT_HOLD refreshes', a late first one
    #[test]
    fn the_camera_cut_holds_its_pictures() {
        // the cut at refresh 10: a picture that same refresh, then the next ones for CUT_HOLD refreshes, then none
        let mut cut = Some((10, false));
        for (refresh, held) in [(10, true), (12, true), (10 + CUT_HOLD, true), (12 + CUT_HOLD, false)] {
            let was;
            (was, cut) = cut_holds(cut, refresh);
            assert_eq!(was, held, "refresh {refresh}");
        }
        assert_eq!(cut, None, "cleared after the hold");
        // the first picture after a cut however late (a map's load), then it clears
        assert_eq!(cut_holds(Some((10, false)), 10 + 5 * CUT_HOLD), (true, None));
        // a picture at the cut's own refresh keeps the cut for the first one after it
        assert_eq!(cut_holds(Some((10, false)), 10), (true, Some((10, false))));
        assert_eq!(cut_holds(None, 10), (false, None));
    }

    /// K = clamp(ceil(monitor Hz x gap / 59.83), 2, 8): 60 Hz stays at one picture in between at 30 fps, 240 at seven
    #[test]
    fn steps_follow_the_monitor() {
        let at = |hz: u32| [2, 3].map(|gap| steps_for(hz, gap));
        assert_eq!(at(60), [2, 3], "60 Hz: as before at 30 fps, and 20 fps scenes get one more");
        assert_eq!(at(59), [2, 3]);
        assert_eq!(at(75), [3, 4]);
        assert_eq!(at(120), [4, 6]);
        assert_eq!(at(144), [5, 8]);
        assert_eq!(at(200), [7, 8]);
        assert_eq!(at(240), [8, 8], "K = 8 at most");
        assert_eq!(at(360), [8, 8]);
        assert_eq!(steps_for(0, 2), 2);
    }

    fn mutex_state(system: &System, handle: u32) -> (Option<ThreadId>, u32) {
        let object = system.kernel.handles.resolve(handle).expect("a mutex handle");
        match system.kernel.objects.get(object) {
            Some(KObject::Mutex(mutex)) => (mutex.owner, mutex.lock_count),
            _ => panic!("not a mutex"),
        }
    }

    /// a holder that took a mutex twice (svcCreateMutex locked plus svcWaitSynchronization1) and a waiter blocked
    /// on it, holder back on the core
    fn holder_and_waiter(system: &mut System) -> (ThreadId, ThreadId, u32) {
        let holder = system.kernel.create_thread("holder", 0x0010_0000, 0x1000_0000, 0, 0x30, 0);
        let waiter = system.kernel.create_thread("waiter", 0x0010_0000, 0x0FF0_0000, 0, 0x30, 0);
        system.map_tls_page(holder);
        system.map_tls_page(waiter);
        system.kernel.schedule(&mut system.cpu, 0);
        assert_eq!(system.kernel.current_thread, Some(holder));
        system.cpu.regs[1] = 1;
        kernel::svc::dispatch(system, 0x13);
        let handle = system.cpu.regs[1];
        let wait = |system: &mut System| {
            system.cpu.regs[0] = handle;
            system.cpu.regs[2] = u32::MAX;
            system.cpu.regs[3] = u32::MAX;
            kernel::svc::dispatch(system, 0x24);
        };
        wait(system);
        assert_eq!(mutex_state(system, handle), (Some(holder), 2));
        system.kernel.current_thread = Some(waiter);
        system.kernel.thread_mut(waiter).status = ThreadStatus::Running;
        system.kernel.thread_mut(holder).status = ThreadStatus::Ready;
        wait(system);
        assert_eq!(system.kernel.thread(waiter).status, ThreadStatus::WaitSync);
        system.kernel.current_thread = Some(holder);
        system.kernel.thread_mut(holder).status = ThreadStatus::Running;
        system.kernel.reschedule_pending = false;
        (holder, waiter, handle)
    }

    /// the holder ended w/ the mutex all given back, the waiter gets it once at the next schedule
    fn waiter_gets_it(system: &mut System, (holder, waiter, handle): (ThreadId, ThreadId, u32)) {
        assert_eq!(system.kernel.thread(holder).status, ThreadStatus::Dead);
        assert_eq!(mutex_state(system, handle), (None, 0));
        let tick = system.cpu.cycles;
        system.kernel.schedule(&mut system.cpu, tick);
        assert_eq!(system.kernel.current_thread, Some(waiter), "{}", system.kernel.describe_wait(waiter));
        assert!(matches!(system.kernel.thread(waiter).wait_result, Some(WaitResult::Signaled(0))));
        assert_eq!(mutex_state(system, handle), (Some(waiter), 1));
    }

    #[test]
    fn user_dir_overrides_the_data_dir() {
        let data = std::path::PathBuf::from("data");
        let plain = System::new(Config { data_dir: Some(data.clone()), ..Default::default() });
        assert_eq!(plain.services.fs.user_dir, data.join("user"));
        let own = data.join("characters/00000000000000ab/user");
        let character = System::new(Config { data_dir: Some(data), user_dir: Some(own.clone()), ..Default::default() });
        assert_eq!(character.services.fs.user_dir, own);
    }

    #[test]
    fn svc_exit_thread_lets_go_of_the_mutexes() {
        let mut system = System::new(Config::default());
        let threads = holder_and_waiter(&mut system);
        kernel::svc::dispatch(&mut system, 0x09);
        waiter_gets_it(&mut system, threads);
    }

    #[test]
    fn returning_from_the_entry_point_lets_go_of_the_mutexes() {
        let mut system = System::new(Config::default());
        let threads = holder_and_waiter(&mut system);
        system.cpu.regs[15] = THREAD_EXIT_MAGIC;
        assert_eq!(system.step(None), StepOutcome::Ran);
        waiter_gets_it(&mut system, threads);
    }

    #[test]
    fn a_branch_to_unmapped_memory_lets_go_of_the_mutexes() {
        let mut system = System::new(Config::default());
        let threads = holder_and_waiter(&mut system);
        system.cpu.regs[15] = 0x0000_1000;
        assert!(!system.memory.is_executable(0x0000_1000));
        assert_eq!(system.step(None), StepOutcome::Ran);
        waiter_gets_it(&mut system, threads);
    }

    /// one thread at create_thread's entry, its code a branch to itself, current and running
    fn one_spinning_thread(system: &mut System) -> ThreadId {
        one_thread_running(system, &[0xEAFF_FFFE])
    }

    /// one thread running code at 0x00100000, current, stack and r0 in the same page (stack at 0x00100F00, r0 at 0x00100800)
    fn one_thread_running(system: &mut System, code: &[u32]) -> ThreadId {
        use memory::{MemoryState, Permission};
        let block = system.memory.phys.allocate(memory::MemoryRegion::Application, 0x1000).unwrap();
        system.memory.map(0x0010_0000, block.addr, 0x1000, Permission::RW | Permission::EXECUTE, MemoryState::Code);
        for (index, word) in code.iter().enumerate() {
            system.memory.write32(0x0010_0000 + index as u32 * 4, *word);
        }
        let id = system.kernel.create_thread("main", 0x0010_0000, 0x0010_0F00, 0, 0x30, 0);
        system.map_tls_page(id);
        system.kernel.schedule(&mut system.cpu, 0);
        assert_eq!(system.kernel.current_thread, Some(id));
        system.cpu.regs[0] = 0x0010_0800;
        id
    }

    /// runs steps till the run reaches tick or steps run out, returns how many it took
    fn steps_to(system: &mut System, tick: u64, steps: usize) -> usize {
        (0..steps).take_while(|_| {
            system.step(None);
            system.cpu.cycles < tick
        }).count() + 1
    }

    /// a thread alone on the core spinning on nothing waits for the next event (next audio frame here)
    /// instead of the interpreter running every turn till then
    #[test]
    fn a_thread_spinning_alone_skips_to_the_next_event() {
        let mut system = System::new(Config::default());
        one_spinning_thread(&mut system);
        let audio = system.next_audio_frame;
        assert!(steps_to(&mut system, audio, 40_000) < 20_000, "skipped");
        assert_eq!(system.cpu.cycles, audio, "to the audio frame, not past it");
        assert!(system.idle.skipped > 0);
    }

    /// a loop counting in memory changes smth every turn and runs every one
    #[test]
    fn a_loop_that_counts_is_not_skipped() {
        let mut system = System::new(Config::default());
        // ldr r1, [r0], add r1, r1, #1, str r1, [r0], b back
        one_thread_running(&mut system, &[0xE590_1000, 0xE281_1001, 0xE580_1000, 0xEAFF_FFFB]);
        for _ in 0..40_000 {
            system.step(None);
        }
        assert_eq!(system.idle.skipped, 0);
        assert_eq!(system.memory.read32(0x0010_0800), 10_000, "every turn counted");
    }

    /// a loop that puts back what it wrote (like taking and dropping a lock) plus push and pop changes nothing and gets skipped
    #[test]
    fn a_loop_that_puts_back_what_it_wrote_is_skipped() {
        let mut system = System::new(Config::default());
        // ldr r1, [r0], add r1, r1, #1, str r1, [r0], sub r1, r1, #1,
        // str r1, [r0], push {r4, lr}, pop {r4, lr}, b back
        let code = [0xE590_1000, 0xE281_1001, 0xE580_1000, 0xE241_1001, 0xE580_1000, 0xE92D_4010, 0xE8BD_4010, 0xEAFF_FFF7];
        one_thread_running(&mut system, &code);
        system.memory.write32(0x0010_0800, 7);
        let audio = system.next_audio_frame;
        assert!(steps_to(&mut system, audio, 40_000) < 20_000, "skipped");
        assert_eq!(system.memory.read32(0x0010_0800), 7);
    }

    /// a skip stops where a sleeping thread wakes, it gets the core then
    #[test]
    fn a_skip_stops_where_a_sleeping_thread_wakes() {
        let mut system = System::new(Config::default());
        one_spinning_thread(&mut system);
        let sleeper = system.kernel.create_thread("sleeper", 0x0010_0000, 0x0010_0E00, 0, 0x20, 0);
        system.map_tls_page(sleeper);
        let thread = system.kernel.thread_mut(sleeper);
        thread.status = ThreadStatus::Sleeping;
        thread.wakeup_at = Some(20_000);
        let steps = (0..40_000).take_while(|_| {
            system.step(None);
            system.kernel.current_thread != Some(sleeper)
        });
        assert!(steps.count() < 20_000, "skipped");
        assert!((20_000..20_008).contains(&system.cpu.cycles), "woke at {}", system.cpu.cycles);
    }

    /// waits on a handle forever, like svcWaitSynchronization1
    fn wait_on(system: &mut System, handle: u32) {
        system.cpu.regs[0] = handle;
        system.cpu.regs[2] = u32::MAX;
        system.cpu.regs[3] = u32::MAX;
        kernel::svc::dispatch(system, 0x24);
    }

    /// steps till the thread runs again, returns the tick it does
    fn runs_again(system: &mut System, id: ThreadId) -> u64 {
        for _ in 0..1000 {
            if system.kernel.current_thread == Some(id) && system.kernel.thread(id).status == ThreadStatus::Running {
                return system.cpu.cycles;
            }
            system.step(None);
        }
        panic!("the thread never ran again");
    }

    /// a wait the very next schedule ends, on a thread keeping the core bc nothing else can run
    /// returns its result b4 the thread goes on, not the handle it got (overwritten later)
    #[test]
    fn a_wait_the_next_schedule_ends_returns_its_result() {
        let mut system = System::new(Config::default());
        let id = one_spinning_thread(&mut system);
        system.cpu.regs[1] = 0;
        kernel::svc::dispatch(&mut system, 0x17);
        let event = system.cpu.regs[1];
        wait_on(&mut system, event);
        assert_eq!(system.kernel.thread(id).status, ThreadStatus::WaitSync);
        let object = system.kernel.resolve(event).unwrap();
        system.kernel.signal_event(object);
        system.step(None);
        assert_eq!(system.kernel.current_thread, Some(id));
        assert_eq!(system.cpu.regs[0], 0, "the wait's result, not the handle");
    }

    /// the top screen's 2nd buffer is the right eye's only while its format asks for 3D like the console's GSP takes it
    /// bit 5 set, bit 6 clear, and the title gave it one
    #[test]
    fn the_right_eye_shows_only_while_the_title_asks_for_3d() {
        let mut system = System::new(Config::default());
        let config = &mut system.gpu.framebuffers[0];
        config.address_a_left = 0x1830_0000;
        config.address_a_right = 0x1834_6500;
        for (format, right) in [(0x20, true), (0x60, false), (0x40, false), (0x00, false)] {
            system.gpu.framebuffers[0].format = 0x0300 | format | 1;
            assert_eq!(system.draws_in_3d(), right, "format 0x{format:X}");
        }
        system.gpu.framebuffers[0].address_a_right = 0;
        system.gpu.framebuffers[0].format = 0x0321;
        assert!(!system.draws_in_3d(), "no buffer for the right eye");
        // the slider games read moves w/ it
        system.set_slider_3d(0.5);
        let page = system.memory.shared_page_mut();
        assert_eq!((f32::from_le_bytes(page[0x80..0x84].try_into().unwrap()), page[0x84]), (0.5, 0), "the LED not forced off");
    }

    /// a zero sleep w/ nobody else ready is a no op, the caller keeps the core and no pass is due
    /// titles poll a worker in a tight svcSleepThread(0) loop, paying a full pass each time was most of what one such title spent
    #[test]
    fn a_zero_sleep_with_nobody_else_ready_does_not_schedule() {
        let mut system = System::new(Config::default());
        let id = one_spinning_thread(&mut system);
        system.kernel.reschedule_pending = false;
        system.cpu.regs[0] = 0;
        system.cpu.regs[1] = 0;
        kernel::svc::dispatch(&mut system, 0x0A);
        assert_eq!(system.kernel.thread(id).status, ThreadStatus::Running, "it keeps the core");
        assert!(!system.kernel.reschedule_pending, "no pass is due");
        assert_eq!(system.cpu.regs[0], 0);
    }

    /// a zero sleep w/ another thread ready still steps aside, so a title polling a worker keeps handing it the core
    #[test]
    fn a_zero_sleep_with_another_ready_steps_aside() {
        let mut system = System::new(Config::default());
        let first = one_spinning_thread(&mut system);
        let second = system.kernel.create_thread("second", 0x0010_0000, 0x0FF0_0000, 0, 0x30, 0);
        system.map_tls_page(second);
        system.kernel.thread_mut(second).status = ThreadStatus::Ready;
        system.cpu.regs[0] = 0;
        system.cpu.regs[1] = 0;
        kernel::svc::dispatch(&mut system, 0x0A);
        assert_eq!(system.kernel.thread(first).status, ThreadStatus::Sleeping, "it steps aside");
        assert!(system.kernel.reschedule_pending, "the ready thread gets a pass");
    }

    /// a zero sleep while a thread is due to wake still gets the pass then, so it wakes on time not at the next preempt
    #[test]
    fn a_zero_sleep_wakes_a_thread_that_is_due() {
        let mut system = System::new(Config::default());
        let first = one_spinning_thread(&mut system);
        let second = system.kernel.create_thread("second", 0x0010_0000, 0x0FF0_0000, 0, 0x30, 0);
        system.map_tls_page(second);
        let thread = system.kernel.thread_mut(second);
        thread.status = ThreadStatus::Sleeping;
        thread.wakeup_at = Some(system.cpu.cycles);
        system.kernel.reschedule_pending = false;
        system.cpu.regs[0] = 0;
        system.cpu.regs[1] = 0;
        kernel::svc::dispatch(&mut system, 0x0A);
        assert_eq!(system.kernel.thread(first).status, ThreadStatus::Sleeping, "it steps aside for the pass");
        assert!(system.kernel.reschedule_pending, "the sleeper due gets a pass");
    }

    /// a service request makes the thread wait for the reply while a lower thread runs, like on the console
    /// Star Fox 64 3D marks a save job running once the more urgent thread doing it exists,
    /// which must be waiting on its first request by then
    #[test]
    fn a_request_lets_the_threads_below_run_until_the_reply() {
        let mut system = System::new(Config::default());
        let id = one_spinning_thread(&mut system);
        let below = system.kernel.create_thread("below", 0x0010_0000, 0x0FF0_0000, 0, 0x31, 0);
        system.map_tls_page(below);
        let port = system.kernel.objects.insert(KObject::ClientPort("srv:".into()));
        let handle = system.kernel.handles.create(&mut system.kernel.objects, port, "srv:");
        // srv:RegisterClient
        let buffer = system.kernel.thread(id).tls + 0x80;
        system.memory.write32(buffer, 0x0001_0002);
        system.memory.write32(buffer + 4, 0x20);
        system.cpu.regs[0] = handle;
        let asked = system.cpu.cycles;
        kernel::svc::dispatch(&mut system, 0x32);
        assert_eq!((system.cpu.regs[0], system.memory.read32(buffer + 4)), (0, 0), "answered");
        assert_eq!(system.kernel.thread(id).status, ThreadStatus::Sleeping, "waiting for the reply");

        assert!(runs_again(&mut system, below) < asked + 100, "the thread below runs at once");
        let mut steps = 0;
        while system.kernel.current_thread != Some(id) || system.kernel.thread(id).status != ThreadStatus::Running {
            system.step(None);
            steps += 1;
            assert!(steps < 40_000, "the thread never got its reply");
        }
        let due = asked + kernel::svc::REQUEST_TICKS;
        assert!((due..due + PREEMPT_INTERVAL).contains(&system.cpu.cycles), "replied at {}, due at {due}", system.cpu.cycles);
    }

    /// a timer fires at its tick not the next frame, and its waiter wakes then
    /// a pulse timer is gone once it woke a waiter and comes back a period after it was due
    #[test]
    fn a_timer_fires_at_its_tick() {
        let mut system = System::new(Config::default());
        let id = one_spinning_thread(&mut system);
        // a pulse timer due in a ms and every ms after
        system.cpu.regs[1] = 2;
        kernel::svc::dispatch(&mut system, 0x1A);
        let timer = system.cpu.regs[1];
        system.cpu.regs[0] = timer;
        system.cpu.regs[2] = 1_000_000;
        system.cpu.regs[3] = 0;
        system.cpu.regs[1] = 1_000_000;
        system.cpu.regs[4] = 0;
        kernel::svc::dispatch(&mut system, 0x1B);
        let period = kernel::thread::nanos_to_ticks(1_000_000);

        wait_on(&mut system, timer);
        let woke = runs_again(&mut system, id);
        assert!((period..period + PREEMPT_INTERVAL).contains(&woke), "woke at {woke}, due at {period}");
        assert_eq!(system.cpu.regs[0], 0);

        wait_on(&mut system, timer);
        assert_eq!(system.kernel.thread(id).status, ThreadStatus::WaitSync, "the pulse is gone");
        let woke = runs_again(&mut system, id);
        assert!((2 * period..2 * period + PREEMPT_INTERVAL).contains(&woke), "woke at {woke}, due at {}", 2 * period);
    }

    /// a pulse timer due in a ms and every ms after
    fn pulse_timer(system: &mut System) -> u32 {
        system.cpu.regs[1] = 2;
        kernel::svc::dispatch(system, 0x1A);
        let timer = system.cpu.regs[1];
        system.cpu.regs[0] = timer;
        system.cpu.regs[2] = 1_000_000;
        system.cpu.regs[3] = 0;
        system.cpu.regs[1] = 1_000_000;
        system.cpu.regs[4] = 0;
        kernel::svc::dispatch(system, 0x1B);
        timer
    }

    /// a pulse timer releases every waiter when it fires, not just the first, and a fire nobody waits for is lost
    #[test]
    fn a_pulse_timer_wakes_every_waiter_then_clears() {
        let mut system = System::new(Config::default());
        let first = one_spinning_thread(&mut system);
        let second = system.kernel.create_thread("second", 0x0010_0000, 0x0FF0_0000, 0, 0x30, 0);
        system.map_tls_page(second);
        let timer = pulse_timer(&mut system);
        let period = kernel::thread::nanos_to_ticks(1_000_000);
        for id in [first, second] {
            system.kernel.current_thread = Some(id);
            system.kernel.thread_mut(id).status = ThreadStatus::Running;
            wait_on(&mut system, timer);
            assert_eq!(system.kernel.thread(id).status, ThreadStatus::WaitSync);
        }
        system.kernel.current_thread = None;
        while system.cpu.cycles < period {
            system.step(None);
        }
        system.step(None);
        for id in [first, second] {
            assert_ne!(system.kernel.thread(id).status, ThreadStatus::WaitSync, "thread {id} woke");
        }

        // the next fire comes w/ nobody waiting and is gone after
        system.cpu.cycles = 2 * period + 10;
        system.kernel.current_thread = Some(first);
        system.kernel.thread_mut(first).status = ThreadStatus::Running;
        wait_on(&mut system, timer);
        assert_eq!(system.kernel.thread(first).status, ThreadStatus::WaitSync, "the fire nothing waited for is lost");
    }

    /// cancelling a timer that came due b4 the scheduler looked keeps its fire, the console's already happened
    #[test]
    fn cancelling_a_timer_due_keeps_its_fire() {
        let mut system = System::new(Config::default());
        let id = one_spinning_thread(&mut system);
        system.cpu.regs[1] = 0;
        kernel::svc::dispatch(&mut system, 0x1A);
        let timer = system.cpu.regs[1];
        system.cpu.regs[0] = timer;
        system.cpu.regs[2] = 1_000_000;
        system.cpu.regs[3] = 0;
        system.cpu.regs[1] = 0;
        system.cpu.regs[4] = 0;
        kernel::svc::dispatch(&mut system, 0x1B);
        let other = system.kernel.create_thread("other", 0x0010_0000, 0x0FF0_0000, 0, 0x30, 0);
        system.map_tls_page(other);
        wait_on(&mut system, timer);
        // the other thread cancels it after it came due
        system.cpu.cycles = kernel::thread::nanos_to_ticks(1_000_000) + 100;
        system.kernel.current_thread = Some(other);
        system.kernel.thread_mut(other).status = ThreadStatus::Running;
        system.cpu.regs[0] = timer;
        kernel::svc::dispatch(&mut system, 0x1C);
        let tick = system.cpu.cycles;
        system.kernel.schedule(&mut system.cpu, tick);
        assert_ne!(system.kernel.thread(id).status, ThreadStatus::WaitSync, "the waiter woke");
    }

    /// closing a handle while a thread waits on its object neither wakes the thread as if signalled
    /// nor hands the object's slot to the next one made, the object goes once the wait ends
    #[test]
    fn closing_what_a_thread_waits_on_keeps_it_waiting() {
        let mut system = System::new(Config::default());
        let id = one_spinning_thread(&mut system);
        system.cpu.regs[1] = 0;
        kernel::svc::dispatch(&mut system, 0x17);
        let event = system.cpu.regs[1];
        let object = system.kernel.resolve(event).unwrap();
        // for a ms
        system.cpu.regs[0] = event;
        system.cpu.regs[2] = 1_000_000;
        system.cpu.regs[3] = 0;
        kernel::svc::dispatch(&mut system, 0x24);
        let other = system.kernel.create_thread("other", 0x0010_0000, 0x0FF0_0000, 0, 0x30, 0);
        system.map_tls_page(other);
        system.kernel.current_thread = Some(other);
        system.kernel.thread_mut(other).status = ThreadStatus::Running;
        system.cpu.regs[0] = event;
        kernel::svc::dispatch(&mut system, 0x23);
        let tick = system.cpu.cycles;
        system.kernel.schedule(&mut system.cpu, tick);
        assert_eq!(system.kernel.thread(id).status, ThreadStatus::WaitSync, "not woken as if signalled");
        system.cpu.regs[1] = 0;
        kernel::svc::dispatch(&mut system, 0x17);
        assert_ne!(system.kernel.resolve(system.cpu.regs[1]), Some(object));

        let late = kernel::thread::nanos_to_ticks(1_000_000) + 1;
        system.kernel.schedule(&mut system.cpu, late);
        assert!(matches!(system.kernel.thread(id).wait_result, Some(kernel::thread::WaitResult::TimedOut)));
        assert!(system.kernel.objects.get(object).is_none(), "gone after the wait");
    }

    /// a pulse event wakes its waiters when signalled and is gone after, the next wait waits
    #[test]
    fn a_pulse_event_does_not_stay_signalled() {
        let mut system = System::new(Config::default());
        let id = one_spinning_thread(&mut system);
        system.cpu.regs[1] = 2;
        kernel::svc::dispatch(&mut system, 0x17);
        let event = system.cpu.regs[1];
        wait_on(&mut system, event);
        let object = system.kernel.resolve(event).unwrap();
        system.kernel.signal_event(object);
        runs_again(&mut system, id);
        wait_on(&mut system, event);
        assert_eq!(system.kernel.thread(id).status, ThreadStatus::WaitSync);
    }

    /// an arbiter wait's timeout takes r5 as its high word, minus 1 waits forever
    #[test]
    fn an_arbiter_timeout_is_64_bits() {
        let mut system = System::new(Config::default());
        let id = one_spinning_thread(&mut system);
        kernel::svc::dispatch(&mut system, 0x21);
        let arbiter = system.cpu.regs[1];
        let address = system.kernel.thread(id).tls + 0x100;
        system.memory.write32(address, 0);
        for (low, high, wakes) in [(u32::MAX, u32::MAX, false), (0x2A05_F200, 1, true)] {
            system.kernel.thread_mut(id).status = ThreadStatus::Running;
            system.cpu.regs[0] = arbiter;
            system.cpu.regs[1] = address;
            // wait if less than, w/ a timeout
            system.cpu.regs[2] = 3;
            system.cpu.regs[3] = 1;
            system.cpu.regs[4] = low;
            system.cpu.regs[5] = high;
            kernel::svc::dispatch(&mut system, 0x22);
            let wakeup = system.kernel.thread(id).wakeup_at;
            assert_eq!(wakeup.is_some(), wakes);
            if wakes {
                // five secs
                let five = kernel::thread::nanos_to_ticks(5_000_000_000);
                assert!(wakeup.unwrap() >= five, "{wakeup:?}");
            }
        }
    }

    /// a thread whose timed arbiter wait ran out and then ended left its entry on the arbiter
    /// a signal for one waiter skips it for the thread really waiting there, and the dead one stays dead
    #[test]
    fn an_arbiter_signal_wakes_only_threads_still_waiting() {
        let mut system = System::new(Config::default());
        let ended = system.kernel.create_thread("ended", 0x0010_0000, 0x1000_0000, 0, 0x30, 0);
        let signaller = system.kernel.create_thread("signaller", 0x0010_0000, 0x0FF0_0000, 0, 0x30, 0);
        let parked = system.kernel.create_thread("parked", 0x0010_0000, 0x0FE0_0000, 0, 0x30, 0);
        for id in [ended, signaller, parked] {
            system.map_tls_page(id);
        }
        system.kernel.schedule(&mut system.cpu, 0);
        assert_eq!(system.kernel.current_thread, Some(ended));
        kernel::svc::dispatch(&mut system, 0x21);
        let arbiter = system.cpu.regs[1];
        let address = system.kernel.thread(ended).tls + 0x100;
        system.memory.write32(address, 0);
        let arbitrate = |system: &mut System, who: ThreadId, kind: u32| {
            system.kernel.current_thread = Some(who);
            system.kernel.thread_mut(who).status = ThreadStatus::Running;
            system.cpu.regs[0] = arbiter;
            system.cpu.regs[1] = address;
            system.cpu.regs[2] = kind;
            system.cpu.regs[3] = 1;
            system.cpu.regs[4] = 1000;
            kernel::svc::dispatch(system, 0x22);
        };
        // waits if less than 1 for a microsecond and times out
        arbitrate(&mut system, ended, 3);
        system.kernel.current_thread = None;
        system.kernel.schedule(&mut system.cpu, 1_000_000);
        assert!(matches!(system.kernel.thread(ended).wait_result, Some(WaitResult::TimedOut)));
        // then ends
        system.kernel.current_thread = Some(ended);
        system.kernel.thread_mut(ended).status = ThreadStatus::Running;
        kernel::svc::dispatch(&mut system, 0x09);
        // another waits forever, a third signals one waiter
        arbitrate(&mut system, parked, 1);
        assert_eq!(system.kernel.thread(parked).status, ThreadStatus::WaitArbiter);
        arbitrate(&mut system, signaller, 0);
        assert_eq!(system.kernel.thread(ended).status, ThreadStatus::Dead);
        assert_eq!(system.kernel.thread(parked).status, ThreadStatus::Ready);
    }
}
