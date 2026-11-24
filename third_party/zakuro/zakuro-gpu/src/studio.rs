//! The character creator's studio view: the player's draws drawn
//! again with an orbit camera into a target of our own, cleared to transparent.
//! - **Marking.** Each draw's shaded vertices are taken back to the world through the game's own view-projection; the
//!   draw is the player's when most of them lie in a cylinder standing on the player's feet (a flat piece too, unless it
//!   lies on the floor at the feet: that is the game's shadow blob, and the studio draws its own floor). Every other draw
//!   left out, or kept with some of it outside, is logged once per mesh.
//! - **Cache.** The marked draws' registers, shaded vertices and lighting tables are kept per game frame, so the spin is
//!   drawn again from them at any time (the game draws at 30 Hz, the creator spins at 60).
//! - **Redraw.** [`render`] turns the model about the vertical through its feet (the camera orbits the other way),
//!   tips it toward a camera that looks 10 degrees down, and fits a whole turn into the target with one pixel scale.
//!
//! Off unless a host calls [`set`] (only `redex --preview` does): every draw pays one relaxed atomic load.
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::lighting::Tables;
use crate::raster::{self, Resources, Vertex};
use crate::registers::*;
use crate::GpuMemory;

pub type M4 = [[f32; 4]; 4];

/// The target in the GPU's orientation (the 3DS screen lies on its side): 360x480 upright.
pub const WIDTH: u32 = 480;
pub const ROWS: u32 = 360;
/// The closest zoom: the face fills most of the picture.
pub const ZOOM_MAX: f32 = 2.0;
/// Where our target sits in the GPU's address space, past any memory a 3DS has.
const FAKE: u32 = 0x7000_0000;
const DEPTH_AT: u32 = WIDTH * ROWS * 4;
/// The part of the target the whole turn fills at zoom 1.
const FILL: f32 = 0.9;
/// The face, as a share of the model's height above the feet, and where it ends up at the closest zoom (normalized
/// device x, the picture's up; 0 is the middle).
const FACE: f32 = 0.84;
const FACE_AT: f32 = 0.1;
/// The yaws the fit looks at: a whole turn.
const FIT_YAWS: usize = 16;

static ON: AtomicBool = AtomicBool::new(false);
static STATE: Mutex<Option<State>> = Mutex::new(None);

pub struct Config {
    /// world to the GPU's clip space as the game draws (the 3DS's sideways turn times the field camera)
    pub g: M4,
    /// the field camera's right axis in the world: the tilt turns about it
    pub right: [f32; 3],
    pub feet: [f32; 3],
    /// the cylinder on the feet that holds the player, world units
    pub radius: f32,
    pub height: f32,
    /// degrees the model tips toward the camera (the field camera looks 33 degrees down; -23 leaves the studio's 10)
    pub tilt: f32,
    /// the game's viewport in the GPU's orientation, for the fit's pixel aspect
    pub game: (f32, f32),
}

/// Whose a draw is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Player,
    /// some of its vertices are in the cylinder, not all: kept when most are
    Partly { inside: usize, of: usize, kept: bool },
    /// flat on the floor at the feet: the game's shadow blob (the studio draws its own)
    Floor,
    Out,
}

/// Zoom 1's view of the model over a whole turn, in the game camera's normalized device coordinates (x is the
/// picture's up); grows, never shrinks, so the zoom never pumps.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub x0: f32,
    pub x1: f32,
    /// the widest any point gets from the vertical through the feet
    pub half: f32,
    /// world units: the model's top above the feet
    pub top: f32,
}

/// One studio picture, upright.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// RGBA8, top row first; alpha 255 where the model is, else 0 (all channels)
    pub rgba: Vec<u8>,
    /// the floor shadow under the feet: centre x, y and radii x, y in pixels
    pub floor: [f32; 4],
    pub draws: usize,
    pub time: Duration,
    /// the picture the model was drawn in, counted as [`frame_ends`] counts
    pub built: u64,
}

/// A marked draw, kept to draw again.
struct Cached {
    registers: Vec<u32>,
    seen: Vec<Vertex>,
    /// the lighting tables it was drawn with (later draws of the frame may load others)
    tables: Option<Tables>,
}

struct State {
    cfg: Config,
    gi: M4,
    /// the frame being drawn, and the last whole one
    building: Vec<Cached>,
    cached: Vec<Cached>,
    /// some of this frame's model points, for the fit
    points: Vec<[f32; 3]>,
    bounds: Option<Bounds>,
    /// meshes already logged as partly in or out
    logged: HashSet<u64>,
    /// pictures the game showed (frame_end calls), and the one the model was last built in
    ends: u64,
    built: u64,
    /// the next model's fit starts afresh (a new look)
    refit: bool,
    target: Vec<u8>,
}

/// Turns the studio on for this process (or off), forgetting what it had.
pub fn set(cfg: Option<Config>) {
    let mut st = STATE.lock().unwrap_or_else(|e| e.into_inner());
    *st = cfg.and_then(|cfg| {
        let Some(gi) = invert(&cfg.g) else {
            log::warn!("studio: the game's view-projection does not invert; off");
            return None;
        };
        Some(State { gi, building: Vec::new(), cached: Vec::new(), points: Vec::new(), bounds: None, logged: HashSet::new(), ends: 0, built: 0, refit: false, target: Vec::new(), cfg })
    });
    ON.store(st.is_some(), Ordering::Relaxed);
}

#[inline]
pub(crate) fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// A point in the GPU's clip space back to the world, None at w = 0.
fn back(gi: &M4, c: &[f32; 4]) -> Option<[f32; 3]> {
    let p = mul_v(gi, c);
    (p[3].abs() > 1e-9).then(|| [p[0] / p[3], p[1] / p[3], p[2] / p[3]])
}

/// Whose a draw is, from its vertices in the world (None: one that could not be taken back).
pub fn mark(cfg: &Config, world: &[Option<[f32; 3]>]) -> Mark {
    let f = cfg.feet;
    let inside = |p: &[f32; 3]| (p[0] - f[0]).hypot(p[2] - f[2]) <= cfg.radius && (-2.0..=cfg.height).contains(&(p[1] - f[1]));
    let n = world.iter().flatten().filter(|p| inside(p)).count();
    let (lo, hi) = world.iter().flatten().fold((f32::MAX, f32::MIN), |(lo, hi), p| (lo.min(p[1]), hi.max(p[1])));
    // flat (under a unit tall), on the floor and at the feet: the game's shadow blob. Flat higher up (a hat band, a
    // badge) is the player's like any other part; flat on the floor elsewhere is out (and logged)
    if hi - lo < 1.0 && lo - f[1] < 1.0 && n > 0 {
        return Mark::Floor;
    }
    if n == 0 {
        return Mark::Out;
    }
    if n == world.len() {
        return Mark::Player;
    }
    Mark::Partly { inside: n, of: world.len(), kept: 2 * n > world.len() }
}

/// The model's turn for a yaw: about the vertical through the feet, then tipped about the camera's right axis, both
/// about the middle of the body.
pub fn turn(cfg: &Config, yaw: f32) -> M4 {
    let (s, c) = yaw.to_radians().sin_cos();
    let spin = [[c, 0.0, s, 0.0], [0.0, 1.0, 0.0, 0.0], [-s, 0.0, c, 0.0], [0.0, 0.0, 0.0, 1.0]];
    let r = cfg.right;
    let (s, c) = cfg.tilt.to_radians().sin_cos();
    // Rodrigues about the unit axis r
    let k = [[0.0, -r[2], r[1]], [r[2], 0.0, -r[0]], [-r[1], r[0], 0.0]];
    let mut tip = [[0.0f32; 4]; 4];
    for i in 0..3 {
        for j in 0..3 {
            let kk: f32 = (0..3).map(|l| k[i][l] * k[l][j]).sum();
            tip[i][j] = (i == j) as u8 as f32 + s * k[i][j] + (1.0 - c) * kk;
        }
    }
    tip[3][3] = 1.0;
    let m = [cfg.feet[0], cfg.feet[1] + cfg.height / 2.0, cfg.feet[2]];
    mul(&translate(m), &mul(&tip, &mul(&spin, &translate(m.map(|v| -v)))))
}

/// Where a world point lands in the game camera's normalized device coordinates, None behind the eye.
fn project(m: &M4, p: [f32; 3]) -> Option<[f32; 2]> {
    let c = mul_v(m, &[p[0], p[1], p[2], 1.0]);
    (c[3] > 1e-6).then(|| [c[0] / c[3], c[1] / c[3]])
}

/// The point `up` world units above the feet: on the vertical the model turns about, so the same at every yaw.
fn on_axis(cfg: &Config, up: f32) -> Option<[f32; 2]> {
    project(&mul(&cfg.g, &turn(cfg, 0.0)), [cfg.feet[0], cfg.feet[1] + up, cfg.feet[2]])
}

/// Zoom 1's bounds over a whole turn of these points, grown from `old`.
pub fn fit(cfg: &Config, points: &[[f32; 3]], old: Option<Bounds>) -> Option<Bounds> {
    let axis = on_axis(cfg, 0.0)?[1];
    let f = cfg.feet;
    let mut b = old.unwrap_or(Bounds { x0: f32::MAX, x1: f32::MIN, half: 0.0, top: 0.0 });
    for p in points {
        b.top = b.top.max(p[1] - f[1]);
    }
    for k in 0..FIT_YAWS {
        let m = mul(&cfg.g, &turn(cfg, k as f32 * 360.0 / FIT_YAWS as f32));
        for q in points.iter().filter_map(|p| project(&m, *p)) {
            (b.x0, b.x1, b.half) = (b.x0.min(q[0]), b.x1.max(q[0]), b.half.max((q[1] - axis).abs()));
        }
    }
    (b.x0 < b.x1 && b.half > 0.0).then_some(b)
}

/// The 2D fit after the game's camera: a whole turn fills the target at zoom 1; closer zooms scale about a point that
/// moves from the body's middle to the face.
pub fn lens(cfg: &Config, b: &Bounds, zoom: f32) -> M4 {
    let (gw, gh) = cfg.game;
    let (tw, th) = (WIDTH as f32, ROWS as f32);
    // the game's pixels the turn spans, then one scale for both axes (target pixels per game pixel)
    let (pw, ph) = ((b.x1 - b.x0) * gw / 2.0, b.half * gh);
    let f = FILL * (tw / pw).min(th / ph);
    let (sx, sy) = (f * gw / tw, f * gh / th);
    let (cx, cy) = ((b.x0 + b.x1) / 2.0, on_axis(cfg, 0.0).map_or(0.0, |a| a[1]));
    // the focus slides from the body's middle (where zoom 1 has it, 0) to the face, which goes to FACE_AT
    let s = ((zoom - 1.0) / (ZOOM_MAX - 1.0)).clamp(0.0, 1.0);
    let face = on_axis(cfg, FACE * b.top).map_or([0.0; 2], |p| [sx * (p[0] - cx), sy * (p[1] - cy)]);
    let (fx, fy, to) = (s * face[0], s * face[1], s * FACE_AT);
    [
        [zoom * sx, 0.0, 0.0, -zoom * sx * cx - zoom * fx + to],
        [0.0, zoom * sy, 0.0, -zoom * sy * cy - zoom * fy],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ]
}

/// The world to our target's clip space at this yaw and zoom.
pub fn orbit(cfg: &Config, b: &Bounds, yaw: f32, zoom: f32) -> M4 {
    mul(&lens(cfg, b, zoom), &mul(&cfg.g, &turn(cfg, yaw)))
}

/// A real draw just made, its shaded vertices in draw order: the player's are kept for [`render`].
pub(crate) fn draw(registers: &[u32], seen: Vec<Vertex>, tables: &Tables, mesh: u64) {
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(st) = guard.as_mut() else { return };
    let world: Vec<Option<[f32; 3]>> = seen.iter().map(|v| back(&st.gi, &raster::studio_clip(v))).collect();
    let m = mark(&st.cfg, &world);
    // every draw left out or only partly in is logged once per mesh (the shadow blob alone goes without a word)
    match m {
        Mark::Partly { inside, of, kept } if st.logged.insert(mesh) => {
            log::info!("studio: mesh {mesh:016X} partly in the player's cylinder ({inside} of {of} vertices), {}", if kept { "kept" } else { "left out" });
        }
        Mark::Out if st.logged.insert(mesh) => log::info!("studio: mesh {mesh:016X} outside the player's cylinder ({} vertices), left out", world.len()),
        _ => {}
    }
    if !matches!(m, Mark::Player | Mark::Partly { kept: true, .. }) {
        return;
    }
    st.points.extend(world.iter().flatten().step_by(3));
    // lighting enabled (GPUREG_LIGHTING_ENABLE): its tables are this draw's
    let lit = registers[0x8F] & 1 != 0;
    st.building.push(Cached { registers: registers.to_vec(), seen, tables: lit.then(|| tables.clone()) });
}

/// The game showed a picture: the draws marked since are the model now.
pub(crate) fn frame_end() {
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(st) = guard.as_mut() else { return };
    st.ends += 1;
    // the other screen's transfer, or a picture without the player: the model stays as it was
    if st.building.is_empty() {
        return;
    }
    (st.cached, st.built) = (std::mem::take(&mut st.building), st.ends);
    let points = std::mem::take(&mut st.points);
    // a new look's first fit starts afresh; one that finds nothing to fit leaves the old bounds and tries again next time
    if let Some(b) = fit(&st.cfg, &points, if st.refit { None } else { st.bounds }) {
        (st.bounds, st.refit) = (Some(b), false);
    }
}

/// The pictures the game has shown so far: a model in a [`Frame`] whose `built` is above the count taken at a look's
/// rebuild was drawn after it.
pub fn frame_ends() -> u64 {
    STATE.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map_or(0, |st| st.ends)
}

/// A new look: the next model's fit starts afresh instead of growing from the last look's.
pub fn refit() {
    if let Some(st) = STATE.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        st.refit = true;
    }
}

/// The model from the last frame the game drew, at `yaw` degrees (0: facing us) and `zoom` (1 to ZOOM_MAX), or why not.
pub(crate) fn render<M: GpuMemory>(memory: &mut M, resources: &mut Resources, yaw: f32, zoom: f32) -> Result<Frame, &'static str> {
    #[cfg(feature = "vulkan")]
    if resources.hardware.is_some() {
        return Err("the studio draws in software only");
    }
    let start = Instant::now();
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let st = guard.as_mut().ok_or("the studio is off")?;
    let b = st.bounds.filter(|_| !st.cached.is_empty()).ok_or("no player draws found")?;
    let to_target = orbit(&st.cfg, &b, yaw, zoom.clamp(1.0, ZOOM_MAX));
    let c = mul(&to_target, &st.gi);
    // a test that keeps the nearer by smaller values starts from the far end
    let mask = st.cached[0].registers[REG_DEPTH_COLOR_MASK];
    let clear = if matches!((mask >> 4) & 7, 6 | 7) { 0 } else { 0x00FF_FFFF };
    st.target.clear();
    st.target.resize((DEPTH_AT + WIDTH * ROWS * 4) as usize, 0);
    for d in st.target[DEPTH_AT as usize..].chunks_exact_mut(4) {
        d.copy_from_slice(&u32::to_le_bytes(clear));
    }
    for d in &mut st.cached {
        let mut regs = std::mem::take(&mut d.registers);
        let kept = [REG_COLOR_BUFFER_ADDRESS, REG_DEPTH_BUFFER_ADDRESS, REG_COLOR_BUFFER_FORMAT, REG_DEPTH_BUFFER_FORMAT, REG_FRAMEBUFFER_DIMENSIONS, REG_VIEWPORT_WIDTH, REG_VIEWPORT_HEIGHT, REG_VIEWPORT_WIDTH_RECIPROCAL, REG_VIEWPORT_HEIGHT_RECIPROCAL, REG_VIEWPORT_XY].map(|r| regs[r]);
        regs[REG_COLOR_BUFFER_ADDRESS] = FAKE >> 3;
        regs[REG_DEPTH_BUFFER_ADDRESS] = (FAKE + DEPTH_AT) >> 3;
        // RGBA8 and D24S8
        regs[REG_COLOR_BUFFER_FORMAT] = 0x0000_0002;
        regs[REG_DEPTH_BUFFER_FORMAT] = 3;
        regs[REG_FRAMEBUFFER_DIMENSIONS] = (regs[REG_FRAMEBUFFER_DIMENSIONS] & (1 << 24)) | ((ROWS - 1) << 12) | WIDTH;
        regs[REG_VIEWPORT_WIDTH] = float24(WIDTH as f32 / 2.0);
        regs[REG_VIEWPORT_HEIGHT] = float24(ROWS as f32 / 2.0);
        regs[REG_VIEWPORT_WIDTH_RECIPROCAL] = float31(2.0 / WIDTH as f32);
        regs[REG_VIEWPORT_HEIGHT_RECIPROCAL] = float31(2.0 / ROWS as f32);
        regs[REG_VIEWPORT_XY] = 0;
        if let Some(t) = d.tables.as_mut() {
            std::mem::swap(&mut resources.light_tables, t);
        }
        // shortcut: the lights stay in the game's view space; the field's light gives ambient only (diffuse and
        // specular 0), so a turned light draws the same pixels. Turn them if a lit state is ever used.
        let mut target = Redirect { inner: &mut *memory, target: &mut st.target };
        raster::studio_redraw(&regs, &mut target, resources, &d.seen, &c);
        if let Some(t) = d.tables.as_mut() {
            std::mem::swap(&mut resources.light_tables, t);
        }
        for (r, v) in [REG_COLOR_BUFFER_ADDRESS, REG_DEPTH_BUFFER_ADDRESS, REG_COLOR_BUFFER_FORMAT, REG_DEPTH_BUFFER_FORMAT, REG_FRAMEBUFFER_DIMENSIONS, REG_VIEWPORT_WIDTH, REG_VIEWPORT_HEIGHT, REG_VIEWPORT_WIDTH_RECIPROCAL, REG_VIEWPORT_HEIGHT_RECIPROCAL, REG_VIEWPORT_XY].into_iter().zip(kept) {
            regs[r] = v;
        }
        d.registers = regs;
    }
    let rgba = upright(&st.target, clear);
    let floor = floor(&st.cfg, &b, &to_target);
    Ok(Frame { width: ROWS, height: WIDTH, rgba, floor, draws: st.cached.len(), time: start.elapsed(), built: st.built })
}

/// The target upright (turned a quarter anticlockwise), alpha from the depth it covered.
fn upright(t: &[u8], clear: u32) -> Vec<u8> {
    let (w, h) = (ROWS as usize, WIDTH as usize);
    let mut out = vec![0u8; w * h * 4];
    for uy in 0..h {
        for ux in 0..w {
            let o = crate::morton(WIDTH - 1 - uy as u32, ux as u32, WIDTH, 4);
            let d = DEPTH_AT as usize + o;
            if u32::from_le_bytes([t[d], t[d + 1], t[d + 2], 0]) == clear {
                continue;
            }
            // RGBA8 lies A, B, G, R in memory
            out[(uy * w + ux) * 4..][..4].copy_from_slice(&[t[o + 3], t[o + 2], t[o + 1], 255]);
        }
    }
    out
}

/// The floor shadow: a circle on the floor around the feet, in upright pixels. Sized by the height, not the reach (a
/// ponytail or a bag would make it wide).
fn floor(cfg: &Config, b: &Bounds, to_target: &M4) -> [f32; 4] {
    let r = b.top * 0.22;
    let (mut x0, mut x1, mut y0, mut y1) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
    for k in 0..32 {
        let (s, c) = (k as f32 * std::f32::consts::TAU / 32.0).sin_cos();
        let p = mul_v(to_target, &[cfg.feet[0] + r * c, cfg.feet[1], cfg.feet[2] + r * s, 1.0]);
        if p[3] <= 1e-6 {
            continue;
        }
        // device x is the picture's up; y across it
        let (uy, ux) = ((1.0 - p[0] / p[3]) * WIDTH as f32 / 2.0, (1.0 + p[1] / p[3]) * ROWS as f32 / 2.0);
        (x0, x1, y0, y1) = (x0.min(ux), x1.max(ux), y0.min(uy), y1.max(uy));
    }
    [(x0 + x1) / 2.0, (y0 + y1) / 2.0, (x1 - x0) / 2.0, (y1 - y0) / 2.0]
}

/// guest memory, but our target in place of the range at FAKE
struct Redirect<'a, M: GpuMemory> {
    inner: &'a mut M,
    target: &'a mut Vec<u8>,
}

impl<M: GpuMemory> Redirect<'_, M> {
    fn ours(&self, addr: u32, len: usize) -> Option<std::ops::Range<usize>> {
        let at = addr.checked_sub(FAKE)? as usize;
        (at + len <= self.target.len()).then_some(at..at + len)
    }
}

impl<M: GpuMemory> GpuMemory for Redirect<'_, M> {
    fn read(&mut self, addr: u32, out: &mut [u8]) {
        match self.ours(addr, out.len()) {
            Some(r) => out.copy_from_slice(&self.target[r]),
            None => self.inner.read(addr, out),
        }
    }
    fn write(&mut self, addr: u32, data: &[u8]) {
        match self.ours(addr, data.len()) {
            Some(r) => self.target[r].copy_from_slice(data),
            None => self.inner.write(addr, data),
        }
    }
    fn slice(&mut self, addr: u32, len: usize) -> Option<&[u8]> {
        match self.ours(addr, len) {
            Some(r) => Some(&self.target[r]),
            None => self.inner.slice(addr, len),
        }
    }
    fn slice_mut(&mut self, addr: u32, len: usize) -> Option<&mut [u8]> {
        match self.ours(addr, len) {
            Some(r) => Some(&mut self.target[r]),
            None => self.inner.slice_mut(addr, len),
        }
    }
    fn translate(&self, paddr: u32) -> u32 {
        if paddr >= FAKE { paddr } else { self.inner.translate(paddr) }
    }
    fn writes(&self) -> Option<u64> {
        self.inner.writes()
    }
}

fn translate(t: [f32; 3]) -> M4 {
    [[1.0, 0.0, 0.0, t[0]], [0.0, 1.0, 0.0, t[1]], [0.0, 0.0, 1.0, t[2]], [0.0, 0.0, 0.0, 1.0]]
}

pub fn mul(a: &M4, b: &M4) -> M4 {
    std::array::from_fn(|i| std::array::from_fn(|j| (0..4).map(|k| a[i][k] * b[k][j]).sum()))
}

pub fn mul_v(a: &M4, v: &[f32; 4]) -> [f32; 4] {
    std::array::from_fn(|i| (0..4).map(|k| a[i][k] * v[k]).sum())
}

/// Gauss-Jordan in f64
pub fn invert(m: &M4) -> Option<M4> {
    let mut a = [[0f64; 8]; 4];
    for i in 0..4 {
        for j in 0..4 {
            a[i][j] = m[i][j] as f64;
        }
        a[i][4 + i] = 1.0;
    }
    for col in 0..4 {
        let pivot = (col..4).max_by(|&x, &y| a[x][col].abs().total_cmp(&a[y][col].abs()))?;
        if a[pivot][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, pivot);
        let p = a[col][col];
        a[col].iter_mut().for_each(|v| *v /= p);
        for row in 0..4 {
            if row != col {
                let f = a[row][col];
                let r = a[col];
                a[row].iter_mut().zip(r).for_each(|(v, r)| *v -= f * r);
            }
        }
    }
    Some(std::array::from_fn(|i| std::array::from_fn(|j| a[i][4 + j] as f32)))
}

/// float24 (1.7.16, bias 63) as the viewport registers hold it
fn float24(v: f32) -> u32 {
    if v == 0.0 {
        return 0;
    }
    let b = v.to_bits();
    let e = ((b >> 23) & 0xFF) as i32 - 127 + 63;
    ((b >> 31) << 23) | ((e.clamp(0, 127) as u32) << 16) | ((b >> 7) & 0xFFFF)
}

/// float31 (1.7.23, bias 63), the reciprocals
fn float31(v: f32) -> u32 {
    let b = v.to_bits();
    let e = ((b >> 23) & 0xFF) as i32 - 127 + 63;
    ((b >> 31) << 30) | ((e.clamp(0, 127) as u32) << 23) | (b & 0x7F_FFFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A camera looking down -z from (0, 20, 100) at the feet on the origin, a 3DS-ish perspective, and the GPU's
    /// sideways turn (x_gpu = y, y_gpu = -x).
    fn cfg() -> Config {
        let (n, f) = (10.0f32, 1000.0f32);
        let fov = 1.0f32 / (0.5f32).tan();
        let aspect = 400.0 / 240.0;
        let proj: M4 = [[fov / aspect, 0.0, 0.0, 0.0], [0.0, fov, 0.0, 0.0], [0.0, 0.0, (f + n) / (n - f), 2.0 * f * n / (n - f)], [0.0, 0.0, -1.0, 0.0]];
        let view = translate([0.0, -20.0, -100.0]);
        let a: M4 = [[0.0, 1.0, 0.0, 0.0], [-1.0, 0.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]];
        Config { g: mul(&a, &mul(&proj, &view)), right: [1.0, 0.0, 0.0], feet: [0.0, 0.0, 0.0], radius: 18.0, height: 48.0, tilt: 0.0, game: (240.0, 400.0) }
    }

    fn apply(m: &M4, p: [f32; 3]) -> [f32; 3] {
        let c = mul_v(m, &[p[0], p[1], p[2], 1.0]);
        [c[0] / c[3], c[1] / c[3], c[2] / c[3]]
    }

    fn near(a: [f32; 3], b: [f32; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-3)
    }

    /// a box of points from the feet up, `w` wide, `h` tall, its middle at (x, z)
    fn body(x: f32, z: f32, w: f32, h: f32) -> Vec<[f32; 3]> {
        (0..=4).flat_map(|i| (0..=4).flat_map(move |j| (0..=4).map(move |k| [x - w / 2.0 + w * i as f32 / 4.0, h * j as f32 / 4.0, z - w / 2.0 + w * k as f32 / 4.0]))).collect()
    }

    #[test]
    fn the_player_is_marked_and_the_room_is_not() {
        let c = cfg();
        let some = |v: Vec<[f32; 3]>| v.into_iter().map(Some).collect::<Vec<_>>();
        // standing on the feet, 40 tall
        assert_eq!(mark(&c, &some(body(0.0, 0.0, 12.0, 40.0))), Mark::Player);
        // the mirror's reflection: the same model 90 units behind
        assert_eq!(mark(&c, &some(body(0.0, -90.0, 12.0, 40.0))), Mark::Out);
        // the game's shadow blob: flat on the floor at the feet, left out without a word
        assert_eq!(mark(&c, &some(body(0.0, 0.0, 12.0, 0.4))), Mark::Floor);
        // flat but up on the body (a hat band, a badge): the player's
        let band: Vec<[f32; 3]> = body(0.0, 0.0, 10.0, 0.5).into_iter().map(|p| [p[0], p[1] + 34.0, p[2]]).collect();
        assert_eq!(mark(&c, &some(band)), Mark::Player);
        // flat on the floor away from the feet (a rug): out, like any draw outside (logged by the caller)
        assert_eq!(mark(&c, &some(body(60.0, 0.0, 12.0, 0.2))), Mark::Out);
        // a piece of furniture reaching into the cylinder: logged, left out
        let mut bed = body(30.0, 0.0, 30.0, 10.0);
        bed.extend(body(12.0, 0.0, 4.0, 10.0).into_iter().take(10));
        match mark(&c, &some(bed)) {
            Mark::Partly { inside, of, kept: false } => assert!(inside > 0 && inside * 2 < of, "{inside}/{of}"),
            m => panic!("{m:?}"),
        }
        // a hair tip past the cylinder's edge: most of it is in, so it is kept (and logged)
        let mut hair = body(0.0, 0.0, 12.0, 40.0);
        hair.extend([[0.0, 30.0, 19.0], [0.0, 31.0, 19.5]]);
        assert!(matches!(mark(&c, &some(hair)), Mark::Partly { kept: true, .. }));
        // a vertex that could not be taken back counts as out; none at all is not the player
        let mut v = some(body(0.0, 0.0, 12.0, 40.0));
        v.push(None);
        assert!(matches!(mark(&c, &v), Mark::Partly { kept: true, .. }));
        assert_eq!(mark(&c, &[]), Mark::Out);
    }

    #[test]
    fn the_turn_spins_about_the_feet_and_tips_toward_the_camera() {
        let mut c = cfg();
        let id = turn(&c, 0.0);
        for p in [[1.0, 2.0, 3.0], [-5.0, 30.0, 4.0]] {
            assert!(near(apply(&id, p), p), "yaw 0, tilt 0 is no turn");
        }
        // a quarter turn: a point in front of the feet goes to the side, at the same height and distance; four come back
        let q = turn(&c, 90.0);
        let p = apply(&q, [0.0, 10.0, 5.0]);
        assert!(near(p, [5.0, 10.0, 0.0]) || near(p, [-5.0, 10.0, 0.0]), "{p:?}");
        let four = mul(&q, &mul(&q, &mul(&q, &q)));
        assert!(near(apply(&four, [3.0, 7.0, -2.0]), [3.0, 7.0, -2.0]));
        // the feet stay on the vertical whatever the yaw
        for yaw in [0.0, 45.0, 200.0] {
            assert!(near(apply(&turn(&c, yaw), c.feet), c.feet), "{yaw}");
        }
        // the tilt turns about the camera's right axis through the body's middle: that axis stays, the head leans
        c.tilt = -23.0;
        let t = turn(&c, 0.0);
        let middle = [0.0, c.height / 2.0, 0.0];
        assert!(near(apply(&t, [7.0, middle[1], 0.0]), [7.0, middle[1], 0.0]));
        let head = apply(&t, [0.0, 40.0, 0.0]);
        assert!(head[2].abs() > 1.0 && (head[1] - 40.0).abs() < 2.5, "{head:?}");
        // and every yaw tips the same way
        assert!(near(apply(&turn(&c, 123.0), [0.0, 40.0, 0.0]), head));
    }

    #[test]
    fn a_whole_turn_fills_the_target_and_zoom_goes_to_the_face() {
        let mut c = cfg();
        c.tilt = -23.0;
        // a model wider one way than the other, so the turn matters
        let pts = body(0.0, 0.0, 6.0, 40.0).into_iter().map(|p| [p[0] * 2.0, p[1], p[2]]).collect::<Vec<_>>();
        let b = fit(&c, &pts, None).unwrap();
        assert!((b.top - 40.0).abs() < 1e-3, "{b:?}");
        // zoom 1: at every yaw every point is in the target, and the whole turn reaches the fill on one axis
        let mut most = 0.0f32;
        for k in 0..36 {
            let m = orbit(&c, &b, k as f32 * 10.0, 1.0);
            for p in &pts {
                let q = apply(&m, *p);
                // the fit looks at 16 yaws: one between them may reach a little further
                assert!(q[0].abs() <= FILL * 1.02 && q[1].abs() <= FILL * 1.02, "yaw {} {q:?}", k * 10);
                most = most.max(q[0].abs()).max(q[1].abs());
            }
        }
        assert!(most > FILL - 0.05, "{most}");
        // the feet stand on the middle line across the picture
        assert!(apply(&orbit(&c, &b, 0.0, 1.0), c.feet)[1].abs() < 1e-3);
        // grown, never shrunk: a smaller model keeps the bounds; a taller one grows them
        assert_eq!(fit(&c, &body(0.0, 0.0, 2.0, 20.0), Some(b)), Some(b));
        assert!(fit(&c, &body(0.0, 0.0, 6.0, 46.0), Some(b)).unwrap().x1 > b.x1);
        assert_eq!(fit(&c, &[], None), None);
        // zoom 2 doubles distances about its focus, which sits between the body's middle and the face
        let (one, two) = (orbit(&c, &b, 30.0, 1.0), orbit(&c, &b, 30.0, 2.0));
        let d = |m: &M4| {
            let (a, b) = (apply(m, [0.0, 10.0, 0.0]), apply(m, [0.0, 30.0, 0.0]));
            (a[0] - b[0]).hypot(a[1] - b[1])
        };
        assert!((d(&two) / d(&one) - 2.0).abs() < 1e-3);
        // the closest zoom puts the face at FACE_AT, across the middle
        let face = apply(&orbit(&c, &b, 77.0, ZOOM_MAX), [0.0, FACE * b.top, 0.0]);
        assert!((face[0] - FACE_AT).abs() < 1e-3 && face[1].abs() < 1e-3, "{face:?}");
        // zoom is clamped by the caller; the lens itself is continuous at 1
        let l = lens(&c, &b, 1.0);
        let l2 = lens(&c, &b, 1.0001);
        assert!(l.iter().flatten().zip(l2.iter().flatten()).all(|(a, b)| (a - b).abs() < 1e-3));
    }

    #[test]
    fn encodings_round_trip() {
        for v in [1.0f32, -3.5, 120.0, 0.25] {
            assert_eq!(crate::shader::isa::decode_float24(float24(v)), v);
        }
        let m = [[2.0, 0.0, 0.0, 1.0], [0.0, 0.0, 3.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 0.5, 1.0]];
        let i = invert(&m).unwrap();
        let e = mul(&m, &i);
        for r in 0..4 {
            for c in 0..4 {
                assert!((e[r][c] - (r == c) as u8 as f32).abs() < 1e-5);
            }
        }
    }
}
