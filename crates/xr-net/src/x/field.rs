//! X's field move models, read side (plan 4 Task 1). Layouts and evidence: docs/re/x-field-move-models.md.
//!
//! Everything is reached from DllField's data segment (CRO segment 2), whose address the caller takes from the
//! loader (it moves with the module), through pointers. Nothing here writes game memory.
use crate::Guest;

/// The player snapshot DllField refreshes every field frame (DllField+0x2EB90 fills it): data segment + this.
pub const SNAPSHOT: u32 = 0x7E4;
/// u16 zone (map) id; equals the save's Situation map id.
pub const SNAP_MAP: u32 = 0x18;
/// u16 facing, 0 up, 1 down, 2 left, 3 right (code.bin 0x4A3FA0 turns the move model's direction into it).
pub const SNAP_FACING: u32 = 0x1C;
/// f32 x, y, z world position (one tile is 18 units); the old "0x0818290C" is this field while DllField's data
/// segment sits at 0x081820F0.
pub const SNAP_POS: u32 = 0x38;
/// u32 pointer to the player controller (its class changes with the movement form).
pub const SNAP_CONTROLLER: u32 = 0x14;

/// Controller: u32 pointer to the move-model manager.
pub const CTRL_MANAGER: u32 = 0x08;
/// Controller: u32 pointer to the player's own move model (a pool slot).
pub const CTRL_MOVE_MODEL: u32 = 0x0C;
/// Controller: u32, bit 0x20 while running (B held and moving).
pub const CTRL_RUN: u32 = 0x20;
/// Controller: u8 form: 0 on foot, 1 on the bike, 2 on roller skates (it stays 2 when skating stops, until the player
/// walks with the d-pad or gets on the bike).
pub const CTRL_FORM: u32 = 0x24;
pub const FORM_BIKE: u8 = 1;
pub const FORM_SKATES: u8 = 2;

/// Manager: u32 pointer to the move-model array.
pub const MGR_POOL: u32 = 0x1490;
/// Manager: u16 number of slots.
pub const MGR_COUNT: u32 = 0x1494;
/// The pool X builds (code.bin 0x112CD0); a different count means the offsets do not fit this build.
pub const POOL_SLOTS: u16 = 32;
/// One move model (code.bin class, constructor 0x11EED8).
pub const MM_SIZE: u32 = 0xAA8;
/// Its vtable, in code.bin (fixed address): the loud check that a pointer really is a move model.
pub const MM_VTABLE: u32 = 0x0059_96A8;
/// u32 status: bit 0 = slot in use (0 = free).
pub const MM_STATUS: u32 = 0x04;
/// u32 status 2: bit 0x100000 = left out of the save (the save loop skips it).
pub const MM_STATUS2: u32 = 0x08;
/// u16 object id (0xFF = the player), u16 zone, u16 model.
pub const MM_ID: u32 = 0x0C;
pub const MM_ZONE: u32 = 0x0E;
pub const MM_MODEL: u32 = 0x10;
/// f32 x, y, z world position.
pub const MM_POS: u32 = 0xEC;
/// u8 movement speed code while moving: 0x10 walk, 0x19 run, 0x28 skates, 0x38 bike, 0 standing.
pub const MM_SPEED: u32 = 0x96D;

/// Status 2 bit: the move model is not written to FieldMoveModelSave.
pub const NO_SAVE: u32 = 0x10_0000;

/// Snapshot: u32 pointer to the field context (what the save code.bin 0x433B20 takes; its +0x20 is the manager).
pub const SNAP_FIELDCTX: u32 = 0x08;
/// Snapshot: u32 pointer to the player field object (0x1C8 bytes, DllField+0xA59E0 builds it).
pub const SNAP_PLAYER_OBJ: u32 = 0x10;
/// Field context +0: GameData, whose vtable is this (code.bin; the constructor fills its save-block table near
/// code.bin 0x1164F4).
pub const GAMEDATA_VTABLE: u32 = 0x0059_AE00;
/// GameData + this: u32 pointer to the MyStatus save block (the block table at GameData+0x61A70, entry 17).
pub const GD_MYSTATUS: u32 = 0x61AB4;
pub const MYSTATUS_VTABLE: u32 = 0x0059_ADC0;
/// MyStatus object: the save block's bytes (save offset 0x14000) start here.
pub const MS_DATA: u32 = 0x08;
/// MyStatus: u8 gender (0 boy, 1 girl).
pub const MS_GENDER: u32 = 0x05;
/// MyStatus: the two multiplayer sprite bytes the intro sets with the gender (code.bin 0x4365D4): boy 01 01, girl 00 00.
pub const MS_SPRITE: u32 = 0x06;
/// MyStatus: the 16-byte trainer fashion (save 0x14030), four u32 bit fields.
pub const MS_FASHION: u32 = 0x30;
/// MyStatus: the trainer name (save 0x14048), 13 UTF-16 units, zero-padded.
pub const MS_NAME: u32 = 0x48;
/// MyStatus: the nickname the player's friends use (save 0x14062, 13 UTF-16 units): the intro copies the typed name
/// here, and the friends' nickname scene later replaces it (the 100% save's trainer gets a nickname there).
pub const MS_NICKNAME: u32 = 0x62;

/// Player field object + this: u32 pointer to the field camera block (the manager's +0x18 holds it too).
pub const PFO_CAMERA: u32 = 0xF0;
/// Camera block + this: the view-projection, 16 f32 row-major. DllField+0x745FC stores the view there and
/// DllField+0x7464C multiplies the projection into it in place (code.bin 0x122A68), every field update.
pub const CAM_VIEWPROJ: u32 = 0xB94;
/// World units above a player's feet where a name goes: just over the head (the standing player's sprite reaches
/// 57 px above its feet at 1x; this lands 4 px over its top with the field's usual camera).
pub const LABEL_HEIGHT: f32 = 43.0;
/// The top screen as the player sees it.
pub const SCREEN_W: f32 = 400.0;
pub const SCREEN_H: f32 = 240.0;

/// The wire form of the look (moved to redex-proto, which the world server shares).
pub use redex_proto::appearance::{APPEARANCE_GEN, APPEARANCE_LEN, APPEARANCE_VERSION, Appearance};

/// The player's appearance, read live through the field context's GameData (DllField's data segment at `data`). None
/// outside the field or when a vtable is not the expected class.
pub fn appearance(g: &mut impl Guest, data: u32) -> Option<Appearance> {
    let ctx = g.u32(at(at(data, SNAPSHOT)?, SNAP_FIELDCTX)?).filter(|&p| p != 0)?;
    let gd = g.u32(ctx)?;
    let d = mystatus_of(g, gd)?;
    let mut fashion = [0; 16];
    g.read(at(d, MS_FASHION)?, &mut fashion).then_some(())?;
    Some(Appearance { gender: g.u8(at(d, MS_GENDER)?)?, fashion })
}

/// MyStatus's data block (the save's 0x14000 bytes) of the GameData at `gamedata`; None when a vtable is not the
/// expected class.
pub fn mystatus_of(g: &mut impl Guest, gamedata: u32) -> Option<u32> {
    if g.u32(gamedata)? != GAMEDATA_VTABLE {
        return None;
    }
    let ms = g.u32(at(gamedata, GD_MYSTATUS)?)?;
    if g.u32(ms)? != MYSTATUS_VTABLE {
        return None;
    }
    at(ms, MS_DATA)
}

/// The field camera's view-projection: clip = m * (x, y, z, 1), rows x, y, z, w; x is the top screen's 400-pixel
/// axis and y its 240-pixel axis as the player sees them (the game rotates for the 3DS's sideways framebuffer later,
/// in the matrix it gives the GPU, not here).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewProj {
    pub m: [[f32; 4]; 4],
}

impl ViewProj {
    /// A world position to top-screen pixels at 1x: (0, 0) top left, x right, y down, 400x240. None behind the camera.
    /// Points off the screen come back as they fall (outside 0..400, 0..240).
    pub fn project(&self, p: [f32; 3]) -> Option<[f32; 2]> {
        let c: [f32; 4] = std::array::from_fn(|r| self.m[r][0] * p[0] + self.m[r][1] * p[1] + self.m[r][2] * p[2] + self.m[r][3]);
        if !(c[3] > 1e-3) {
            return None;
        }
        Some([(1.0 + c[0] / c[3]) * SCREEN_W / 2.0, (1.0 - c[1] / c[3]) * SCREEN_H / 2.0])
    }

    /// Where the camera is: the one point the x, y and w rows all send to 0 (the view's origin). None for a matrix
    /// that has no such point.
    pub fn eye(&self) -> Option<[f32; 3]> {
        let r = [0, 1, 3].map(|r| self.m[r].map(f64::from));
        let det = |a: [f64; 3], b: [f64; 3], c: [f64; 3]| a[0] * (b[1] * c[2] - b[2] * c[1]) - a[1] * (b[0] * c[2] - b[2] * c[0]) + a[2] * (b[0] * c[1] - b[1] * c[0]);
        let col = |i: usize| [r[0][i], r[1][i], r[2][i]];
        let rhs = [-r[0][3], -r[1][3], -r[2][3]];
        let d = det(col(0), col(1), col(2));
        if d.abs() < 1e-9 {
            return None;
        }
        // Cramer's rule, by columns
        Some([det(rhs, col(1), col(2)) / d, det(col(0), rhs, col(2)) / d, det(col(0), col(1), rhs) / d].map(|v| v as f32))
    }

    /// Degrees the view turned from `self` to `now` (the w rows' direction parts are unit vectors, see camera: their
    /// dot is the turn's cosine).
    fn turn_to(&self, now: &ViewProj) -> f32 {
        let cos = (0..3).map(|i| self.m[3][i] * now.m[3][i]).sum::<f32>();
        cos.clamp(-1.0, 1.0).acos().to_degrees()
    }
}

/// Frame interpolation's cut hint: a field camera whose move breaks from its own last move by more than this cut
/// elsewhere (three tiles; following the player the bike moves it at most 11 units a field update, a door's warp
/// thousands).
pub const CUT_MOVE: f32 = 54.0;
/// ... or whose turn breaks from its last turn by more than this many degrees (following the player it never turns).
pub const CUT_TURN: f32 = 8.0;

/// The field camera from change to change, for frame interpolation's cut hint (research doc section 3 M3): a cut is a
/// change that breaks from the camera's own motion (like the draw matcher's jumps), so a scripted camera panning or
/// turning fast at a steady pace is no cut; one starting at full speed from standing is one, once.
#[derive(Debug, Default, Clone)]
pub struct CameraCuts {
    last: Option<ViewProj>,
    /// the eye's move and the turn at the last change, none at first and after a cut
    motion: Option<([f32; 3], f32)>,
    /// a cut's own move and turn, till the change after it: that one is a cut too only when it's itself a jump unlike
    /// the cut's (a warp set in two steps), not when it goes on like it (a pan that started at full speed) or is small
    /// (the new map's camera following the player)
    cut: Option<([f32; 3], f32)>,
}

impl CameraCuts {
    /// The camera read this refresh; whether it cut elsewhere since the last change. A camera that did not change (the
    /// field updates every other refresh, or stands) changes nothing.
    pub fn see(&mut self, now: ViewProj) -> bool {
        let Some(last) = self.last.replace(now).filter(|last| *last != now) else { return false };
        let (moved, turned) = match (last.eye(), now.eye()) {
            (Some(a), Some(b)) => (std::array::from_fn(|i| b[i] - a[i]), last.turn_to(&now)),
            _ => ([f32::INFINITY; 3], f32::INFINITY),
        };
        let breaks = |(before, turned_before): ([f32; 3], f32)| {
            let apart = (0..3).map(|i| (moved[i] - before[i]).powi(2)).sum::<f32>().sqrt();
            !(apart <= CUT_MOVE && (turned - turned_before).abs() <= CUT_TURN)
        };
        let cut = match (self.motion, self.cut) {
            (Some(motion), _) => breaks(motion),
            (None, Some(cut)) => breaks(([0.0; 3], 0.0)) && breaks(cut),
            (None, None) => false,
        };
        (self.motion, self.cut) = if cut { (None, Some((moved, turned))) } else { (Some((moved, turned)), None) };
        cut
    }
}

/// The field camera, read through the player field object. None outside the field, or when the block does not look
/// like a perspective view-projection (finite, with a w row whose direction part is a unit vector: -forward).
pub fn camera(g: &mut impl Guest, data: u32) -> Option<ViewProj> {
    let pfo = g.u32(at(at(data, SNAPSHOT)?, SNAP_PLAYER_OBJ)?).filter(|&p| p != 0)?;
    let cam = g.u32(at(pfo, PFO_CAMERA)?).filter(|&p| p != 0)?;
    let base = at(cam, CAM_VIEWPROJ)?;
    let mut m = [[0f32; 4]; 4];
    for (r, row) in m.iter_mut().enumerate() {
        for (c, v) in row.iter_mut().enumerate() {
            *v = g.f32(at(base, 16 * r as u32 + 4 * c as u32)?)?;
        }
    }
    let len = |r: usize| (m[r][0] * m[r][0] + m[r][1] * m[r][1] + m[r][2] * m[r][2]).sqrt();
    // rows x and y are the view's right and up scaled by the projection: their ratio is the screen's 400:240
    let ok = m.iter().flatten().all(|v| v.is_finite()) && (len(3) - 1.0).abs() < 1e-3 && len(0) > 0.0 && (len(1) / len(0) - SCREEN_W / SCREEN_H).abs() < 0.01;
    ok.then_some(ViewProj { m })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Facing {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Walk,
    Run,
    Skates,
    Bike,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Player {
    pub map: u16,
    pub facing: Facing,
    pub pos: [f32; 3],
    pub mode: Mode,
    pub moving: bool,
    /// the player's move model (a pool slot)
    pub move_model: u32,
    /// the move-model manager
    pub manager: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Slot {
    pub addr: u32,
    pub id: u16,
    pub zone: u16,
    pub model: u16,
    pub saved: bool,
}

/// `p + off`, or None past the end of the 32-bit address space (a garbage pointer, not a wrapped read).
fn at(p: u32, off: u32) -> Option<u32> {
    p.checked_add(off)
}

/// The player, read through DllField's data segment at `data`. None outside the field (DllField not loaded, or the
/// player not built yet) or when a pointer does not lead to a move model.
pub fn player(g: &mut impl Guest, data: u32) -> Option<Player> {
    let snap = at(data, SNAPSHOT)?;
    let ctrl = g.u32(at(snap, SNAP_CONTROLLER)?).filter(|&p| p != 0)?;
    let (manager, mm) = (g.u32(at(ctrl, CTRL_MANAGER)?)?, g.u32(at(ctrl, CTRL_MOVE_MODEL)?)?);
    if g.u32(mm)? != MM_VTABLE {
        return None;
    }
    let facing = match g.u16(at(snap, SNAP_FACING)?)? {
        0 => Facing::Up,
        1 => Facing::Down,
        2 => Facing::Left,
        3 => Facing::Right,
        _ => return None,
    };
    let form = g.u8(at(ctrl, CTRL_FORM)?)?;
    let mode = if form == FORM_SKATES {
        Mode::Skates
    } else if form == FORM_BIKE {
        Mode::Bike
    } else if g.u32(at(ctrl, CTRL_RUN)?)? & 0x20 != 0 {
        Mode::Run
    } else {
        Mode::Walk
    };
    let p = at(snap, SNAP_POS)?;
    let pos = [g.f32(p)?, g.f32(at(p, 4)?)?, g.f32(at(p, 8)?)?];
    // during some scripted events the snapshot is all zeros (map 0, position 0,0,0) while the player stands elsewhere:
    // no answer then (map 0 is Vaniville, a real map)
    if pos == [0.0; 3] {
        return None;
    }
    let moving = g.u8(at(mm, MM_SPEED)?)? != 0;
    Some(Player { map: g.u16(at(snap, SNAP_MAP)?)?, facing, pos, mode, moving, move_model: mm, manager })
}

/// The pool's slots in use. None when the manager's array is not the 32 move models this build makes.
pub fn slots(g: &mut impl Guest, manager: u32) -> Option<Vec<Slot>> {
    let (base, count) = (g.u32(at(manager, MGR_POOL)?)?, g.u16(at(manager, MGR_COUNT)?)?);
    if count != POOL_SLOTS {
        return None;
    }
    let mut used = Vec::new();
    for i in 0..u32::from(count) {
        let a = at(base, i * MM_SIZE)?;
        if g.u32(a)? != MM_VTABLE {
            return None;
        }
        if g.u32(at(a, MM_STATUS)?)? & 1 != 0 {
            let saved = g.u32(at(a, MM_STATUS2)?)? & NO_SAVE == 0;
            let (id, zone, model) = (g.u16(at(a, MM_ID)?)?, g.u16(at(a, MM_ZONE)?)?, g.u16(at(a, MM_MODEL)?)?);
            used.push(Slot { addr: a, id, zone, model, saved });
        }
    }
    Some(used)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A guest with a few words set: the layout walk above, without the game.
    struct Words(std::collections::HashMap<u32, u8>);
    impl Guest for Words {
        fn read(&mut self, addr: u32, out: &mut [u8]) -> bool {
            for (i, b) in out.iter_mut().enumerate() {
                *b = *self.0.get(&(addr + i as u32)).unwrap_or(&0);
            }
            true
        }
    }
    impl Words {
        fn put(&mut self, addr: u32, bytes: &[u8]) {
            for (i, b) in bytes.iter().enumerate() {
                self.0.insert(addr + i as u32, *b);
            }
        }
    }

    #[test]
    fn camera_projects_and_finds_its_eye() {
        let pv: [f32; 16] = [
            2.239_229, 0.0, 0.0, -19_367.172, 0.0, 2.827_694_7, -2.435_636_5, 21_783.193, 0.0, 0.663_239_2, 0.769_999, -7_355.513_7, 0.0, -0.652_627_35, -0.757_679, 7_269.825_7,
        ];
        let (data, pfo, cam) = (0x0818_20F0u32, 0x0824_C970u32, 0x0827_CC10u32);
        let mut g = Words(Default::default());
        g.put(data + SNAPSHOT + SNAP_PLAYER_OBJ, &pfo.to_le_bytes());
        g.put(pfo + PFO_CAMERA, &cam.to_le_bytes());
        for (i, v) in pv.iter().enumerate() {
            g.put(cam + CAM_VIEWPROJ + 4 * i as u32, &v.to_le_bytes());
        }
        let c = camera(&mut g, data).unwrap();
        let near = |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).abs() < 0.1 && (a[1] - b[1]).abs() < 0.1;
        assert!(near(c.project([8649.0, 144.0, 9135.0]).unwrap(), [199.94, 147.90]));
        assert!(near(c.project([8595.0, 146.0, 9189.0]).unwrap(), [85.97, 224.62]));
        // behind the camera (its eye is at about z 9317): no point
        assert_eq!(c.project([8649.0, 144.0, 9600.0]), None);
        let eye = c.eye().unwrap();
        assert!((0..3).all(|i| (eye[i] - [8649.04, 322.07, 9317.45][i]).abs() < 0.05), "{eye:?}");
        let moved = |d: [f32; 3]| ViewProj { m: c.m.map(|r| [r[0], r[1], r[2], r[3] - (r[0] * d[0] + r[1] * d[1] + r[2] * d[2])]) };
        assert!((0..3).all(|i| (moved([11.0, 0.0, -3.0]).eye().unwrap()[i] - (eye[i] + [11.0, 0.0, -3.0][i])).abs() < 0.05));
        // turned about the eye, around the world's up (the view pitches down, so it turns less than the yaw)
        let turned = |deg: f32| {
            let (s, k) = deg.to_radians().sin_cos();
            let dot = |r: [f32; 4]| r[0] * eye[0] + r[1] * eye[1] + r[2] * eye[2];
            ViewProj { m: c.m.map(|r| {
                let t = [r[0] * k + r[2] * s, r[1], r[2] * k - r[0] * s, 0.0];
                [t[0], t[1], t[2], r[3] + dot(r) - dot(t)]
            }) }
        };
        assert!(turned(12.0).eye().is_some_and(|e| (0..3).all(|i| (e[i] - eye[i]).abs() < 0.05)));
        assert!((c.turn_to(&turned(12.0)) - 9.09).abs() < 0.05, "pitched down, a 12 degree yaw turns the view 9");

        // the cut hint, change by change (the field writes the camera every other refresh: an unchanged one is skipped)
        let seen = |steps: &[ViewProj]| {
            let mut cuts = CameraCuts::default();
            steps.iter().map(|&v| cuts.see(v)).collect::<Vec<bool>>()
        };
        let pan = |step: f32, n: usize| (0..n).map(|i| moved([step * i as f32, 0.0, 0.0])).collect::<Vec<_>>();
        // following the player: no cut; a walk standing between steps: none
        assert!(seen(&pan(11.0, 8)).iter().all(|&cut| !cut));
        assert!(seen(&[c, c, moved([4.0, 0.0, 0.0]), moved([4.0, 0.0, 0.0]), moved([8.0, 0.0, 0.0])]).iter().all(|&cut| !cut));
        // a scripted camera easing into a fast pan (90 units a change): no cut
        let ease: Vec<ViewProj> = [0.0, 10.0, 40.0, 90.0, 180.0, 270.0, 360.0, 450.0].map(|x| moved([x, 0.0, 0.0])).to_vec();
        assert!(seen(&ease).iter().all(|&cut| !cut));
        // one starting at full speed from standing: its first change only (once a change set the camera's motion)
        assert_eq!(seen(&[c, moved([2.0, 0.0, 0.0]), moved([92.0, 0.0, 0.0]), moved([182.0, 0.0, 0.0]), moved([272.0, 0.0, 0.0])]), [false, false, true, false, false]);
        assert!(seen(&pan(90.0, 5)).iter().all(|&cut| !cut), "the first change sets the motion");
        // a door's warp: a cut, then the new map's camera following the player is none (the cut set no motion)
        assert_eq!(seen(&[c, moved([2.0, 0.0, 0.0]), moved([11965.0, 0.0, 0.0]), moved([11967.0, 0.0, 0.0])]), [false, false, true, false]);
        // turns: a steady 10 degrees a change is a pan, a sudden 12 one a cut
        let yaws: Vec<ViewProj> = [0.0, 3.0, 9.0, 19.0, 29.0, 39.0].map(turned).to_vec();
        assert!(seen(&yaws).iter().all(|&cut| !cut), "{:?}", seen(&yaws));
        assert_eq!(seen(&[c, turned(1.0), turned(14.0)]), [false, false, true]);
        // a warp, its map's camera, then a second warp: both cut
        assert_eq!(seen(&[c, moved([2.0, 0.0, 0.0]), moved([5000.0, 0.0, 0.0]), moved([5002.0, 0.0, 0.0]), moved([5004.0, 0.0, 0.0]), moved([-3000.0, 0.0, 0.0])]), [false, false, true, false, false, true]);
        // a warp set in two steps: both cut, and the camera following the player after them is none (league.txt 15402)
        assert_eq!(seen(&[c, moved([2.0, 0.0, 0.0]), moved([5000.0, 0.0, 0.0]), moved([9000.0, 0.0, 0.0]), moved([9002.0, 0.0, 0.0]), moved([9010.0, 0.0, 0.0])]), [false, false, true, true, false, false]);
        // a block that is not a view-projection (zeros, or rows that do not keep the 400:240 screen): no camera
        g.put(cam + CAM_VIEWPROJ, &0.0f32.to_le_bytes());
        assert_eq!(camera(&mut g, data), None);
        g.put(cam + CAM_VIEWPROJ, &3.732f32.to_le_bytes());
        assert_eq!(camera(&mut g, data), None);
        g.put(cam + CAM_VIEWPROJ, &pv[0].to_le_bytes());
        g.put(cam + CAM_VIEWPROJ + 48, &f32::NAN.to_le_bytes());
        assert_eq!(camera(&mut g, data), None);
    }

    #[test]
    fn walks_snapshot_controller_and_pool() {
        let (data, ctrl, mgr, pool): (u32, u32, u32, u32) = (0x0818_20F0, 0x082B_D8B4, 0x08D0_1F5C, 0x08D0_3924);
        let mut g = Words(Default::default());
        let snap = data + SNAPSHOT;
        g.put(snap + SNAP_MAP, &200u16.to_le_bytes());
        g.put(snap + SNAP_FACING, &3u16.to_le_bytes());
        for (i, v) in [8667.0f32, 144.0, 9135.0].iter().enumerate() {
            g.put(snap + SNAP_POS + 4 * i as u32, &v.to_le_bytes());
        }
        g.put(snap + SNAP_CONTROLLER, &ctrl.to_le_bytes());
        let player_mm = pool + 10 * MM_SIZE;
        g.put(ctrl + CTRL_MANAGER, &mgr.to_le_bytes());
        g.put(ctrl + CTRL_MOVE_MODEL, &player_mm.to_le_bytes());
        g.put(ctrl + CTRL_RUN, &0x20u32.to_le_bytes());
        g.put(mgr + MGR_POOL, &pool.to_le_bytes());
        g.put(mgr + MGR_COUNT, &32u16.to_le_bytes());
        for i in 0..32 {
            g.put(pool + i * MM_SIZE, &MM_VTABLE.to_le_bytes());
        }
        g.put(player_mm + MM_STATUS, &0x0001_0443u32.to_le_bytes());
        g.put(player_mm + MM_ID, &[0xFF, 0, 200, 0, 2, 0]);
        g.put(player_mm + MM_SPEED, &[0x19]);
        g.put(pool + MM_STATUS, &1u32.to_le_bytes());
        g.put(pool + MM_STATUS2, &NO_SAVE.to_le_bytes());

        // the bike's form wins over the run bit, as skates' does
        g.put(ctrl + CTRL_FORM, &[FORM_BIKE]);
        assert_eq!(player(&mut g, data).unwrap().mode, Mode::Bike);
        g.put(ctrl + CTRL_FORM, &[FORM_SKATES]);
        assert_eq!(player(&mut g, data).unwrap().mode, Mode::Skates);
        g.put(ctrl + CTRL_FORM, &[0]);
        let p = player(&mut g, data).unwrap();
        assert_eq!((p.map, p.facing, p.mode, p.moving, p.move_model), (200, Facing::Right, Mode::Run, true, player_mm));
        assert_eq!(p.pos, [8667.0, 144.0, 9135.0]);
        let s = slots(&mut g, p.manager).unwrap();
        assert_eq!(s.len(), 2);
        assert!(!s[0].saved && s[1].saved && s[1].id == 0xFF && s[1].zone == 200);

        // the all-zero snapshot of a scripted event: no answer
        g.put(snap + SNAP_POS, &[0; 12]);
        assert!(player(&mut g, data).is_none());
        g.put(snap + SNAP_POS, &8667.0f32.to_le_bytes());
        // a pointer that is not a move model, and a pool of another size: no answer rather than a wrong one
        g.put(pool + 10 * MM_SIZE, &0u32.to_le_bytes());
        assert!(player(&mut g, data).is_none());
        g.put(mgr + MGR_COUNT, &31u16.to_le_bytes());
        assert!(slots(&mut g, mgr).is_none());
        // a garbage pointer near the top of the address space: no answer, not a read that wrapped to address 0
        g.put(mgr + MGR_COUNT, &32u16.to_le_bytes());
        g.put(mgr + MGR_POOL, &0xFFFF_FF00u32.to_le_bytes());
        assert!(slots(&mut g, mgr).is_none());
        assert!(player(&mut g, u32::MAX - 4).is_none());
    }

    /// Fashion bytes as saves hold them (save 0x14030): the 100% save's boy, the new-game boy "Red" (first look), and
    /// a new-game girl (third look).
    const TOBIN: [u8; 16] = [0x88, 0xB9, 0x19, 0x05, 0xD0, 0x09, 0x32, 0x03, 0x11, 0, 0, 0, 0, 0, 0, 0];
    const RED: [u8; 16] = [0x48, 0x64, 0x09, 0x01, 0x01, 0xCB, 0x08, 0x03, 0x41, 0, 0, 0, 0, 0, 0, 0];
    const GIRL3: [u8; 16] = [0xC0, 0x00, 0x24, 0x02, 0x4C, 0x05, 0x90, 0x02, 0x00, 0x04, 0, 0, 0, 0, 0, 0];

    #[test]
    fn appearance_reads_through_gamedata() {
        let (data, ctx, gd, ms) = (0x0818_20F0u32, 0x08C6_705Cu32, 0x08C6_727Cu32, 0x08C7_9C24u32);
        let mut g = Words(Default::default());
        g.put(data + SNAPSHOT + SNAP_FIELDCTX, &ctx.to_le_bytes());
        g.put(ctx, &gd.to_le_bytes());
        g.put(gd, &GAMEDATA_VTABLE.to_le_bytes());
        g.put(gd + GD_MYSTATUS, &ms.to_le_bytes());
        g.put(ms, &MYSTATUS_VTABLE.to_le_bytes());
        g.put(ms + MS_DATA + MS_FASHION, &GIRL3);
        g.put(ms + MS_DATA + MS_GENDER, &[1]);
        assert_eq!(appearance(&mut g, data), Some(Appearance { gender: 1, fashion: GIRL3 }));
        // not MyStatus, not GameData: no answer
        g.put(ms, &0u32.to_le_bytes());
        assert_eq!(appearance(&mut g, data), None);
        g.put(ms, &MYSTATUS_VTABLE.to_le_bytes());
        g.put(gd, &0u32.to_le_bytes());
        assert_eq!(appearance(&mut g, data), None);
    }

    #[test]
    fn appearance_validates_like_the_game() {
        for (gender, f) in [(0, TOBIN), (0, RED), (1, GIRL3)] {
            let a = Appearance { gender, fashion: f };
            assert!(a.validate(), "{a:?}");
            let b = a.to_bytes();
            assert_eq!((b.len(), b[0], b[1]), (APPEARANCE_LEN, APPEARANCE_GEN, APPEARANCE_VERSION));
            assert_eq!(Appearance::from_bytes(&b), Some(a));
        }
        let set = |f: [u8; 16], word: usize, at: u32, bits: u32, v: u32| {
            let mut a = Appearance { gender: 0, fashion: f };
            let mut w = a.words();
            w[word] = (w[word] & !(((1u32 << bits) - 1) << at)) | (v << at);
            for (i, x) in w.iter().enumerate() {
                a.fashion[4 * i..4 * i + 4].copy_from_slice(&x.to_le_bytes());
            }
            a
        };
        // the boy's hat goes to 23, the girl's to 31 (her field is a bit wider)
        assert!(set(TOBIN, 0, 11, 5, 23).validate() && !set(TOBIN, 0, 11, 5, 24).validate());
        // skin: only the looks seen in saves (1 to 3); 0 is not checked by the game and was never seen
        assert!(set(TOBIN, 0, 6, 2, 1).validate() && set(TOBIN, 0, 6, 2, 3).validate());
        assert!(!set(TOBIN, 0, 6, 2, 0).validate());
        assert!(!Appearance { gender: 1, ..set(GIRL3, 0, 6, 2, 0) }.validate());
        assert!(Appearance { gender: 1, ..set(GIRL3, 0, 11, 6, 31) }.validate());
        assert!(!Appearance { gender: 1, ..set(GIRL3, 0, 11, 6, 32) }.validate());
        // a boy's fashion with a girl's gender (bits 3-5 are 1 for a boy, 0 for a girl), an unknown gender
        assert!(!Appearance { gender: 1, fashion: TOBIN }.validate());
        assert!(!Appearance { gender: 0, fashion: GIRL3 }.validate());
        assert!(!Appearance { gender: 2, fashion: TOBIN }.validate());
        // a bit the game never reads (word 3 for a boy, the top of word 1)
        assert!(!set(TOBIN, 3, 0, 1, 1).validate());
        assert!(!set(TOBIN, 1, 31, 1, 1).validate());
        // hostile bytes: wrong tag, version or length, or a valid frame around an invalid outfit
        let mut b = Appearance { gender: 0, fashion: TOBIN }.to_bytes();
        assert_eq!(Appearance::from_bytes(&b[..18]), None);
        assert_eq!(Appearance::from_bytes(&[b.as_slice(), &[0]].concat()), None);
        b[0] = 5;
        assert_eq!(Appearance::from_bytes(&b), None);
        b[0] = APPEARANCE_GEN;
        b[1] = 2;
        assert_eq!(Appearance::from_bytes(&b), None);
        assert_eq!(Appearance::from_bytes(&set(TOBIN, 1, 0, 6, 36).to_bytes()), None);
        assert_eq!(Appearance::from_bytes(&[0xFF; 64]), None);
    }

    /// The view-projection DllField held outside the Laverre City Pokemon Center (the 100% save, a tile left of the
    /// door): the player's feet land on the sprite's feet (199.9, 147.9; the screen shows them at about 200, 151) and
    /// an NPC's at 86.0, 224.6 (seen at about 86, 229).
    #[test]
    fn camera_projects_onto_the_top_screen() {
        let pv: [f32; 16] = [
            2.239_229, 0.0, 0.0, -19_367.172, 0.0, 2.827_694_7, -2.435_636_5, 21_783.193, 0.0, 0.663_239_2, 0.769_999, -7_355.513_7, 0.0, -0.652_627_35, -0.757_679, 7_269.825_7,
        ];
        let (data, pfo, cam) = (0x0818_20F0u32, 0x0824_C970u32, 0x0827_CC10u32);
        let mut g = Words(Default::default());
        g.put(data + SNAPSHOT + SNAP_PLAYER_OBJ, &pfo.to_le_bytes());
        g.put(pfo + PFO_CAMERA, &cam.to_le_bytes());
        for (i, v) in pv.iter().enumerate() {
            g.put(cam + CAM_VIEWPROJ + 4 * i as u32, &v.to_le_bytes());
        }
        let c = camera(&mut g, data).unwrap();
        let near = |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).abs() < 0.1 && (a[1] - b[1]).abs() < 0.1;
        assert!(near(c.project([8649.0, 144.0, 9135.0]).unwrap(), [199.94, 147.90]));
        assert!(near(c.project([8595.0, 146.0, 9189.0]).unwrap(), [85.97, 224.62]));
        // a name LABEL_HEIGHT over the feet sits just above the player's sprite (y 91..158 on this frame)
        let head = c.project([8649.0, 144.0 + LABEL_HEIGHT, 9135.0]).unwrap();
        assert!((head[0] - 199.94).abs() < 0.1 && (84.0..91.0).contains(&head[1]), "{head:?}");
        // behind the camera (its eye is at about z 9317): no point
        assert_eq!(c.project([8649.0, 144.0, 9600.0]), None);
        // a block that is not a view-projection (zeros, or rows that do not keep the 400:240 screen): no camera
        g.put(cam + CAM_VIEWPROJ, &0.0f32.to_le_bytes());
        assert_eq!(camera(&mut g, data), None);
        g.put(cam + CAM_VIEWPROJ, &3.732f32.to_le_bytes());
        assert_eq!(camera(&mut g, data), None);
        g.put(cam + CAM_VIEWPROJ, &pv[0].to_le_bytes());
        g.put(cam + CAM_VIEWPROJ + 48, &f32::NAN.to_le_bytes());
        assert_eq!(camera(&mut g, data), None);
    }
}
