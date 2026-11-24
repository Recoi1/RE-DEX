//! XR_INBETWEENS=DIR, headless: Smooth motion's pictures in between as a window at XR_SMOOTH_HZ Hz (default 240) would
//! show them, saved for checks (the endpoint test, monotonic motion, GIF strips). For
//! each new top picture between script frames XR_INBETWEENS_FROM and XR_INBETWEENS_TO (default all):
//! `DIR/NNNNNN-k.png`, k = 1..K-1 in between and K the game's own (and `NNNNNN-k-bottom.png`, the bottom screen as the
//! window shows it w/ each; K = 1 for a picture shown w/o any, so strips show what a window does), and a row of
//! `DIR/inbetweens.csv` (XR_INBETWEENS_MAX=N: the first N pictures, then the run ends; the script clock stands still while
//! `autobattle` fights: XR_INBETWEENS_FROM/_TO are on that clock, the files and rows are named by the frames run, the
//! same apart from autobattle): the frame, the
//! gap, K, the field camera's view-projection (16, row-major) and the player's model's position (xr-net's read accessors,
//! after the frame; empty outside the field). With ZAKURO_TWIN_ENDPOINT=1 the last picture in between is drawn at
//! t = 1, and each is compared byte for byte with the game's own (logged).
//! The screens are drawn on a device of the run's own and shown straight from it, as a window's are. Display only:
//! nothing here writes game memory.

use std::io::Write;
use std::path::PathBuf;

use crate::exec::Machine;
use zakuro_common::Screen;

pub struct Inbetweens {
    dir: PathBuf,
    from: u64,
    to: u64,
    max: u64,
    hz: u32,
    endpoint: bool,
    csv: std::io::BufWriter<std::fs::File>,
    /// the last top picture saved (Cadence::pictures), and the endpoint comparisons so far: the same, differing
    last: Option<u64>,
    same: (u32, u32),
    /// pictures saved so far
    saved: u64,
}

impl Inbetweens {
    pub fn from_env() -> Result<Option<Inbetweens>, String> {
        let Some(dir) = std::env::var_os("XR_INBETWEENS").map(PathBuf::from) else { return Ok(None) };
        let number = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<u64>().ok());
        std::fs::create_dir_all(&dir).map_err(|e| format!("{dir:?}: {e}"))?;
        let file = std::fs::File::create(dir.join("inbetweens.csv")).map_err(|e| format!("{dir:?}: {e}"))?;
        let mut csv = std::io::BufWriter::new(file);
        let head: Vec<String> = (0..16).map(|i| format!("vp{i}")).chain(["x", "y", "z"].map(String::from)).collect();
        writeln!(csv, "frame,gap,steps,{}", head.join(",")).map_err(|e| e.to_string())?;
        Ok(Some(Inbetweens {
            dir,
            from: number("XR_INBETWEENS_FROM").unwrap_or(0),
            to: number("XR_INBETWEENS_TO").unwrap_or(u64::MAX),
            max: number("XR_INBETWEENS_MAX").unwrap_or(u64::MAX),
            hz: number("XR_SMOOTH_HZ").unwrap_or(240) as u32,
            endpoint: std::env::var_os("ZAKURO_TWIN_ENDPOINT").is_some_and(|v| v == "1"),
            csv,
            last: None,
            same: (0, 0),
            saved: 0,
        }))
    }

    /// the console w/ the GPU drawing on a device of the run's own, the screens shown straight from it, Smooth motion on
    pub fn config(config: xr_hle::Config) -> Result<xr_hle::Config, String> {
        let device = std::sync::Arc::new(zakuro_gpu::SharedDevice::new()?);
        Ok(xr_hle::Config { hardware_renderer: true, device: Some(device), interpolate: true, ..config })
    }

    /// once booted: the pictures in between for the monitor rate asked for; XR_SMOOTH_HZ=0 turns them off (the same
    /// run w/o Smooth motion, for the display-only check)
    pub fn start(&self, m: &mut Machine) {
        let s = m.system();
        if self.hz == 0 {
            s.set_interpolation(false);
        } else if !s.gpu.interpolates() {
            crate::log::log("XR_INBETWEENS: frame interpolation is off (no GPU showing the screens?)");
        }
        s.set_motion_rate(self.hz);
        crate::log::log(&format!("XR_INBETWEENS: pictures in between for {} Hz into {}", self.hz, self.dir.display()));
    }

    /// after script frame t (frame n run) ran: a new top picture's pictures in between and its own, w/ the camera and
    /// the player
    pub fn frame(&mut self, m: &mut Machine, t: u64, n: u64) {
        let s = m.system();
        // every 600 frames: what the pictures in between take and K (the VRAM guard's soak)
        if n % 600 == 0 {
            if let Some((bytes, budget, cap)) = s.gpu.twins_memory() {
                let steps = s.motion().map_or(0, |motion| motion.steps);
                crate::log::log(&format!("XR_INBETWEENS: frame {n}: pictures in between take {} MB of {} MB, cap {cap}, now {} (K {steps})", bytes >> 20, budget >> 20, s.gpu.twins()));
            }
        }
        // ZAKURO_INTERP_TRACE=window: the interpolation trace for the frames saved only
        if std::env::var_os("ZAKURO_INTERP_TRACE").is_some_and(|v| v == "window") {
            zakuro_gpu::set_interpolation_trace((self.from..=self.to).contains(&t));
        }
        if t < self.from || t > self.to {
            return;
        }
        if self.saved >= self.max {
            crate::log::stop(0, &format!("XR_INBETWEENS: {} pictures saved (XR_INBETWEENS_MAX), the run ends", self.saved));
        }
        if self.last.replace(s.cadence.pictures) == Some(s.cadence.pictures) {
            return;
        }
        self.saved += 1;
        if std::env::var_os("ZAKURO_INTERP_TRACE").is_some() {
            crate::log::log(&format!("XR_INBETWEENS: frame {n}'s picture"));
        }
        // a picture shown w/o pictures in between: its own alone (K = 1)
        let motion = s.motion().unwrap_or(xr_hle::Motion { picture: 0, gap: 0, steps: 1 });
        let pictures: Vec<Option<xr_hle::Screen>> = (0..motion.steps - 1).map(Some).chain([None]).map(|k| s.upright_screen(Screen::Top, k)).collect();
        // the bottom screen as the window shows it w/ each: its own picture in between where it has one, else its own
        let own: Vec<Option<usize>> = (0..motion.steps - 1).map(|k| s.gpu_between(Screen::Bottom, false, k).map(|_| k)).collect();
        let bottoms: Vec<Option<xr_hle::Screen>> = own.into_iter().chain([None]).map(|k| s.upright_screen(Screen::Bottom, k)).collect();
        let named = pictures.iter().enumerate().map(|(k, p)| (p, format!("{n:06}-{}.png", k + 1)));
        let named = named.chain(bottoms.iter().enumerate().map(|(k, p)| (p, format!("{n:06}-{}-bottom.png", k + 1))));
        for (picture, name) in named {
            let Some((rgba, width, height)) = picture else {
                crate::log::log(&format!("XR_INBETWEENS: frame {n}: no {name} (of K = {})", motion.steps));
                continue;
            };
            let path = self.dir.join(name);
            let saved = std::fs::File::create(&path).map_err(|e| e.to_string()).and_then(|f| {
                let mut e = png::Encoder::new(std::io::BufWriter::new(f), *width, *height);
                e.set_color(png::ColorType::Rgba);
                e.write_header().and_then(|mut w| w.write_image_data(rgba)).map_err(|e| e.to_string())
            });
            if let Err(e) = saved {
                crate::log::log(&format!("{}: {e}", path.display()));
            }
        }
        if self.endpoint && motion.steps > 1 {
            let [.., last, own] = &pictures[..] else { return };
            let same = last.is_some() && last == own;
            if same { self.same.0 += 1 } else { self.same.1 += 1 }
            crate::log::log(&format!("XR_INBETWEENS: frame {n}: the picture in between at t = 1 is {} the game's own ({} the same, {} not so far)", if same { "byte for byte" } else { "NOT" }, self.same.0, self.same.1));
        }
        let data = s.cro.modules.iter().rev().find(|m| m.name == "DllField").and_then(|m| m.segments.get(2)).copied();
        let mut guest = crate::Mem(&mut s.memory);
        let camera = data.and_then(|data| xr_net::x::field::camera(&mut guest, data));
        let player = data.and_then(|data| xr_net::x::field::player(&mut guest, data));
        let vp: Vec<String> = match camera {
            Some(c) => c.m.iter().flatten().map(|v| v.to_string()).collect(),
            None => vec![String::new(); 16],
        };
        // where the player's model is drawn: its move model's position (the snapshot's lags it while moving)
        let model = player.and_then(|p| {
            let mut at = |o: u32| xr_net::Guest::f32(&mut guest, p.move_model + xr_net::x::field::MM_POS + o);
            Some([at(0)?, at(4)?, at(8)?])
        });
        let pos: Vec<String> = match model {
            Some(pos) => pos.iter().map(|v| v.to_string()).collect(),
            None => vec![String::new(); 3],
        };
        let _ = writeln!(self.csv, "{n},{},{},{},{}", motion.gap, motion.steps, vp.join(","), pos.join(","));
        let _ = self.csv.flush();
    }
}
