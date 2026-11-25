//! XR_FRAMESTATS=1 (or =DIR): the window's frame costs and presents, for the high fps work.
//! Writes `framestats-presents.csv` (one row per present) and `framestats-seconds.csv` (one per second) to DIR, else
//! `<XR_DATA>/logs`, else the current directory, and logs the second's line.

use std::io::Write;
use std::time::{Duration, Instant};
use xr_hle::Cadence;

pub struct FrameStats {
    presents: std::io::BufWriter<std::fs::File>,
    seconds: std::io::BufWriter<std::fs::File>,
    began: Instant,
    /// since the second's start: presents, emulated refreshes, the time in the game (guest code and GPU recording)
    /// and in present (the fence wait, the swapchain, the display's block), the longest present
    count: (u32, u32, Duration, Duration, Duration),
    second: Instant,
    before: Cadence,
    /// presents since the emulated refresh's own
    repeat: u32,
    /// the GPU's time walking command lists (recording the draws) at the last second's end
    busy: Duration,
}

impl FrameStats {
    pub fn from_env() -> Option<FrameStats> {
        let var = std::env::var_os("XR_FRAMESTATS").filter(|v| !v.is_empty() && v != "0")?;
        let dir = if var == "1" {
            std::env::var_os("XR_DATA").map_or_else(std::path::PathBuf::new, |d| std::path::Path::new(&d).join("logs"))
        } else {
            var.into()
        };
        let _ = std::fs::create_dir_all(&dir);
        let open = |name: &str, head: &str| {
            let mut file = std::io::BufWriter::new(std::fs::File::create(dir.join(name)).ok()?);
            writeln!(file, "{head}").ok()?;
            Some(file)
        };
        let presents = open("framestats-presents.csv", "t_ms,kind,picture,repeat,game_ms,present_ms,between,frame,began_ms,backend_ms,steps,pre_ms,waited_ms,t")?;
        let seconds = open(
            "framestats-seconds.csv",
            "t_s,frame,presents,refreshes,game_ms_avg,record_ms_avg,present_ms_avg,present_ms_max,gap1,gap2,gap3,gap4,gap5,gap6,gap7,gap8plus,sets,smooth,draws,matched,moved,jumped,jumping,monitor_hz",
        )?;
        log::info!("XR_FRAMESTATS: writing to {}", dir.display());
        Some(FrameStats { presents, seconds, began: Instant::now(), count: Default::default(), second: Instant::now(), before: Cadence::default(), repeat: 0, busy: Duration::ZERO })
    }

    /// one present: `kind` 0 the same pictures again, 1 a new refresh's (2, a refresh left unshown, was the window thread's
    /// before the emulation got its own); `refreshes` and `game` are the refreshes the emulation thread ran since the last
    /// present (those the window never showed too) and their time, `present` the present call's; `between` is Smooth
    /// motion's picture in between it showed (k + 1), 0 the game's own: picture and between tell distinct pictures apart;
    /// `frame` and `began` the shown set's refresh and when it started (input to present), `backend` the presenter's
    /// own part of `present` (the rest is the interface), `steps` the set's K (0 w/o pictures in between) and `t` where the
    /// picture shown is from the one b4 (1 the game's own),
    /// `pre` the time from the last present to this one's start and `waited` the step's deliberate waits in it
    #[allow(clippy::too_many_arguments)]
    pub fn present(&mut self, kind: u8, picture: u64, between: u8, refreshes: u32, game: Duration, present: Duration, frame: u64, began: Instant, backend: Duration, steps: usize, t_shown: f32, pre: Duration, waited: Duration) {
        let emulated = kind != 0;
        self.repeat = if emulated { 0 } else { self.repeat + 1 };
        let t = self.began.elapsed().as_secs_f64() * 1000.0;
        let began = began.checked_duration_since(self.began).map_or(0.0, ms);
        let _ = writeln!(self.presents, "{t:.3},{kind},{picture},{},{:.3},{:.3},{between},{frame},{began:.3},{:.3},{steps},{:.3},{:.3},{t_shown:.4}", self.repeat, ms(game), ms(present), ms(backend), ms(pre), ms(waited));
        let c = &mut self.count;
        c.0 += (kind != 2) as u32;
        c.1 += refreshes;
        c.2 += game;
        c.3 += present;
        c.4 = c.4.max(present);
    }

    /// once a second, the second's row, with the monitor's rate
    pub fn tick(&mut self, cadence: &Cadence, busy: Duration, monitor_hz: f64) {
        if self.second.elapsed() < Duration::from_secs(1) {
            return;
        }
        let (n, r, game, present, longest) = std::mem::take(&mut self.count);
        let (b, c) = (&self.before, cadence);
        let d = |now: u64, then: u64| now.saturating_sub(then);
        let gaps: Vec<u64> = (1..=8).map(|i| d(c.gaps[i], b.gaps[i])).collect();
        let t = self.began.elapsed().as_secs_f64();
        let avg = |sum: Duration, n: u32| if n == 0 { 0.0 } else { ms(sum) / n as f64 };
        let line = format!(
            "{t:.1},{},{n},{r},{:.3},{:.3},{:.3},{:.3},{},{},{},{},{},{},{},{},{}",
            c.refreshes,
            avg(game, r),
            avg(busy.saturating_sub(self.busy), r),
            avg(present, n),
            ms(longest),
            gaps.iter().map(u64::to_string).collect::<Vec<_>>().join(","),
            d(c.sets, b.sets),
            d(c.smooth, b.smooth),
            d(c.draws, b.draws),
            d(c.matched, b.matched),
            d(c.moved, b.moved),
            d(c.jumped, b.jumped),
            d(c.jumping, b.jumping),
            format_args!("{monitor_hz:.2}")
        );
        let _ = writeln!(self.seconds, "{line}");
        let _ = self.seconds.flush();
        let _ = self.presents.flush();
        log::info!("framestats t={t:.0}s presents {n}/s refreshes {r}/s game {:.2} ms present {:.2} ms (max {:.2}) gaps {gaps:?}", avg(game, r), avg(present, n), ms(longest));
        self.before = cadence.clone();
        self.busy = busy;
        self.second = Instant::now();
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}
