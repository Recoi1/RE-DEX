//! The player's own outfit changed in place, on the player's move model, with no map load
//! (the character creator's live preview). The rebuild sequence:
//! once an outfit's dress-up slot is loaded (state 3), tear the player's draw handler down while the
//! old parameter is still in +0x998, copy the new parameter in (COPY_PARAM) and init the handler; the game builds the
//! new look at its next update.
//!
//! Our outfits are asked for with header byte 3 = 1 ([`PLAYER_MARK`]), the player's own value.
//! The part loader (code.bin 0x46F3F4) reads that byte: 1 loads the player's full part set into one of two player heaps
//! (`[PLAYER_HEAPS]+0x2C` and `+0x30`, 0x4A000 bytes each); 0 loads the NPC set, on which the girl's long hair is drawn
//! as a plank and short hair not at all. With both player heaps taken it reads through a null heap (a fault
//! at 0x00000004). The game's own outfit holds one heap for good (it is never released: a later load into
//! its slot was seen to read a stale pointer), so at most one of ours is loaded at a time:
//! 1. A change from one of ours to another first puts the game's own outfit back on ("home", still loaded), releases
//!    ours TAKE_OFF ticks later, and waits until its slot has unloaded (its heap is free again).
//! 2. Then it asks for the new look (RequestDressUp) and rebuilds the player in it once loaded.
//!
//! The runtime calls [`Wardrobe::tick`] once per pass of the game's main loop, from the per-frame hook.
use super::field::{self, Appearance};
use super::heap;
use super::puppet::{self, Calls, Dll, DRESS_HEAP, DRESS_KEEP, MGR_MODEL_SYS, MM_DRESS, PARAM_SIZE, RELEASE_DRESS, REQUEST_DRESS};

/// code.bin `(dst, src)`: copies a dress-up parameter (header and body; dst's body pointer into the model).
pub const COPY_PARAM: u32 = 0x0046_E1BC;
/// Move model: the draw handler object; vtable +0x0C init, +0x18 teardown.
pub const MM_HANDLER: u32 = 0xA10;
/// Move model: the handler's work area (r1 of those calls).
pub const MM_HANDLER_WORK: u32 = 0x964;
/// Header byte 3 of our outfits: the player's part set (see the module doc).
pub const PLAYER_MARK: u8 = 1;
/// code.bin: pointer to the dress-up loader's globals; its +0x2C and +0x30 are the two player heaps (0: free).
pub const PLAYER_HEAPS: u32 = 0x005E_9EE4;
/// Ticks between putting the game's own outfit back on and releasing ours (the torn-down model is freed at once; this
/// covers the pictures still being drawn).
pub const TAKE_OFF: u32 = 10;

type Bytes = [u8; PARAM_SIZE];

#[derive(Default)]
pub struct Wardrobe {
    /// the newest look wanted, not asked for yet
    want: Option<(u32, Appearance)>,
    /// asked for, waiting for its parts: (seq, look, its parameter's bytes)
    asked: Option<(u32, Appearance, Bytes)>,
    /// ours on the player (its parameter's bytes); None: the game's own
    worn: Option<Bytes>,
    /// the game's own outfit as the player wore it when first seen (the parameter's bytes)
    home: Option<Bytes>,
    /// ours, taken off: its bytes, and the tick it is released at (None: released, its slot unloading)
    off: Option<(Bytes, Option<u32>)>,
    /// ours was taken off for the look wanted: the game's own outfit is on only until that look is
    between: bool,
    /// the model system the slots above are in
    sys: u32,
    ticks: u32,
}

/// A parameter in guest memory at `at` from its bytes (the header, then the body at +0x10, as `puppet::dress` lays it
/// out).
fn put(g: &mut impl Calls, at: u32, b: &Bytes) -> Option<u32> {
    let mut h = [0u8; 12];
    h[..8].copy_from_slice(&b[..8]);
    h[8..].copy_from_slice(&(at + 0x10).to_le_bytes());
    (g.write(at + 0x10, &b[12..]) && g.write(at, &h)).then_some(at)
}

/// Both player heaps taken, or the loader's globals unreadable: a new outfit of ours would fault the loader.
pub(crate) fn player_heaps_full(g: &mut impl Calls) -> bool {
    let Some(base) = g.u32(PLAYER_HEAPS).filter(|&b| b != 0) else { return true };
    ![0x2C, 0x30].iter().any(|o| g.u32(base + o) == Some(0))
}

impl Wardrobe {
    pub fn new() -> Wardrobe {
        Wardrobe::default()
    }

    /// The newest wins: a look wanted earlier and not asked for yet is dropped.
    pub fn want(&mut self, seq: u32, look: Appearance) {
        self.want = Some((seq, look));
    }

    /// The player wears the game's own outfit only on the way from one of ours to the look wanted (not a look anyone
    /// asked for: the preview holds its last picture meanwhile).
    pub fn between(&self) -> bool {
        self.between
    }

    /// Some(seq) on the tick the player was rebuilt in that look (drawn at the game's next update), or on the tick a
    /// look already worn was wanted again.
    pub fn tick(&mut self, g: &mut impl Calls, dll: Dll) -> Option<u32> {
        self.ticks = self.ticks.saturating_add(1);
        let p = field::player(g, dll.data)?;
        let (mm, sys) = (p.move_model, g.u32(p.manager.checked_add(MGR_MODEL_SYS)?).filter(|&s| s != 0)?);
        if sys != self.sys {
            // another model system (a field reload): none of its slots are ours, the player wears the game's outfit
            let want = self.want.or(self.asked.map(|(s, a, _)| (s, a)));
            *self = Wardrobe { want, sys, ticks: self.ticks, ..Wardrobe::default() };
        }
        // the game's own outfit, seen before any rebuild of ours (byte 3 = 1, its slot loaded)
        if self.home.is_none() {
            self.home = Some(puppet::param_bytes(g, mm + MM_DRESS).filter(|b| b[3] == PLAYER_MARK)?);
        }
        let home = self.home?;
        let at = g.scratch() + 0x30;
        let now = self.ticks;
        match self.off {
            Some((b, Some(due))) if now >= due => {
                let pa = put(g, at, &b)?;
                g.call(dll.code + RELEASE_DRESS, &[sys, pa]);
                self.off = Some((b, None));
            }
            // unloaded: its player heap is free again
            Some((b, None)) if puppet::dress_slot(g, sys, &b).is_none() => self.off = None,
            _ => {}
        }
        if let Some((seq, a, bytes)) = self.asked {
            match puppet::dress_slot(g, sys, &bytes) {
                Some((_, request, 3)) if request != 4 => {}
                Some((_, request, state)) if request != 4 && state != 5 => return None, // loading
                // unloading or gone: asked again once a player heap is free (the loader faults without one)
                _ if player_heaps_full(g) => return None,
                _ => {
                    let pa = self.dress(g, at, &a)?;
                    g.call(dll.code + REQUEST_DRESS, &[sys, pa]);
                    return None;
                }
            }
            let pa = put(g, at, &bytes)?;
            self.wear(g, mm, pa, bytes)?;
            self.asked = None;
            return Some(seq);
        }
        let (seq, a) = self.want?;
        // a look the game's check refuses, or one for the other player model (that gender's state), is dropped
        // (and nothing is on the way any more: the game's own outfit, if on, is the look worn)
        if !a.validate() || g.u16(mm + field::MM_MODEL) != Some(puppet::player_model(a.gender)) {
            (self.want, self.between) = (None, false);
            return None;
        }
        let pa = self.dress(g, at, &a)?;
        let bytes = puppet::param_bytes(g, pa)?;
        if self.worn.unwrap_or(home) == bytes {
            (self.want, self.between) = (None, false);
            return Some(seq);
        }
        // the game's own look, or ours to take off first: home, still loaded
        if bytes == home || self.worn.is_some() {
            if let Some(old) = self.worn {
                self.off = Some((old, Some(now.saturating_add(TAKE_OFF))));
            }
            let pa = put(g, at, &home)?;
            self.wear(g, mm, pa, home)?;
            if bytes == home {
                self.want = None;
                return Some(seq);
            }
            self.between = true;
            return None;
        }
        let heap = heap::free(g, &heap::MODEL_HEAPS).is_none_or(|f| f.iter().all(|f| f.largest >= DRESS_HEAP));
        if self.off.is_some() || puppet::free_dress_slots(g, sys) <= DRESS_KEEP || !heap || player_heaps_full(g) {
            return None;
        }
        let pa = self.dress(g, at, &a)?;
        g.call(dll.code + REQUEST_DRESS, &[sys, pa]);
        self.asked = Some((seq, a, bytes));
        self.want = None;
        None
    }

    /// `puppet::dress` with the player's mark.
    fn dress(&self, g: &mut impl Calls, at: u32, a: &Appearance) -> Option<u32> {
        let pa = puppet::dress(g, at, a)?;
        g.write(pa + 3, &[PLAYER_MARK]).then_some(pa)
    }

    /// Rebuilds the player in the parameter at `pa` (its bytes `b`), torn down with the old one still in place.
    fn wear(&mut self, g: &mut impl Calls, mm: u32, pa: u32, b: Bytes) -> Option<()> {
        let h = g.u32(mm + MM_HANDLER).filter(|&h| h != 0)?;
        let vt = g.u32(h)?;
        let (teardown, init) = (g.u32(vt.checked_add(0x18)?).filter(|&f| f != 0)?, g.u32(vt.checked_add(0x0C)?).filter(|&f| f != 0)?);
        let work = mm + MM_HANDLER_WORK;
        g.call(teardown, &[h, work]);
        g.call(COPY_PARAM, &[mm + MM_DRESS, pa]);
        g.call(init, &[h, work]);
        self.worn = (Some(b) != self.home).then_some(b);
        self.between = false;
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::field;
    use super::super::heap;
    use super::super::look::{Look, HAIR_COLOURS};
    use super::super::puppet::*;
    use super::*;
    use crate::Guest;
    use std::collections::HashMap;

    const CODE: u32 = 0x006A_5180;
    const DATA: u32 = 0x0818_20F0;
    const CTRL: u32 = 0x082B_D8B4;
    const MGR: u32 = 0x08D0_1F5C;
    const MM: u32 = 0x08D0_3924;
    const SYS: u32 = 0x0900_0000;
    /// The draw handler and its vtable (DllField's).
    const H: u32 = 0x0820_0000;
    const VT: u32 = 0x007A_B384;
    const TEARDOWN: u32 = CODE + 0xDA8D4;
    const INIT: u32 = CODE + 0xDA710;
    /// The dress-up loader's globals (`[PLAYER_HEAPS]`).
    const LOADER: u32 = 0x0830_0000;

    /// A recording fake: guest memory, the calls made, the model system's 20 dress-up slots and the loader's two
    /// player heaps. RequestDressUp leaves a new outfit loading (state 2, `load` finishes it); ReleaseDressUp leaves it
    /// unloading (state 5, `unload` empties the slot and frees its player heap).
    struct Game {
        mem: HashMap<u32, u8>,
        calls: Vec<(u32, Vec<u32>)>,
        /// header byte 3 of the player's +0x998 at each teardown
        torn_with: Vec<u8>,
        /// the slots our requests took, and the player heap each holds (+0x2C or +0x30)
        ours: HashMap<u32, u32>,
    }

    impl Guest for Game {
        fn read(&mut self, addr: u32, out: &mut [u8]) -> bool {
            for (i, b) in out.iter_mut().enumerate() {
                *b = *self.mem.get(&(addr + i as u32)).unwrap_or(&0);
            }
            true
        }
    }

    impl Calls for Game {
        fn write(&mut self, addr: u32, data: &[u8]) -> bool {
            self.put(addr, data);
            true
        }
        fn call(&mut self, f: u32, args: &[u32]) -> u32 {
            self.calls.push((f, args.to_vec()));
            match f {
                DRESS_BOY | DRESS_GIRL => {
                    let f: Vec<u8> = (0..16).map(|i| self.mem[&(args[1] + i)]).collect();
                    self.put(args[0], &f);
                }
                f if f == CODE + REQUEST_DRESS => {
                    assert_eq!(args[0], SYS);
                    let o = self.outfit(args[1]);
                    if let Some(i) = self.holding(&o) {
                        self.put(self.slot(i), &[0]); // the game's: an unload asked for is cancelled
                    } else if let Some(i) = (0..DRESS_SLOTS).find(|&i| self.u16(self.slot(i)) == Some(0)) {
                        // the player's part set takes a player heap; with none free the real loader faults
                        assert_eq!(o[3], PLAYER_MARK, "our outfits carry the player's mark");
                        let heap = [0x2C, 0x30].into_iter().find(|&h| self.u32(LOADER + h) == Some(0)).expect("both player heaps taken: the loader faults");
                        self.put(LOADER + heap, &1u32.to_le_bytes());
                        self.ours.insert(i, heap);
                        self.put_slot(i, &o, 2);
                    }
                }
                f if f == CODE + RELEASE_DRESS => {
                    let o = self.outfit(args[1]);
                    let i = self.holding(&o).expect("released an outfit no slot holds");
                    assert!(self.ours.contains_key(&i), "released the game's own outfit");
                    self.put(self.slot(i), &[4, 5]);
                }
                TEARDOWN => {
                    assert_eq!(args, [H, MM + MM_HANDLER_WORK]);
                    let b = self.mem[&(MM + MM_DRESS + 3)];
                    self.torn_with.push(b);
                }
                COPY_PARAM => {
                    let o = self.outfit(args[1]);
                    self.put_param(args[0], &o);
                }
                _ => {}
            }
            0
        }
        fn scratch(&self) -> u32 {
            0x0FFF_FE00
        }
    }

    impl Game {
        /// The player in the bedroom with player model `model`, wearing the game's own outfit (the pyjamas-like
        /// default, byte 3 = 1), loaded in dress-up slot 0 and holding the first player heap.
        fn new(model: u16) -> Game {
            let mut g = Game { mem: HashMap::new(), calls: Vec::new(), torn_with: Vec::new(), ours: HashMap::new() };
            let snap = DATA + field::SNAPSHOT;
            g.put(snap + field::SNAP_CONTROLLER, &CTRL.to_le_bytes());
            g.put(CTRL + field::CTRL_MANAGER, &MGR.to_le_bytes());
            g.put(CTRL + field::CTRL_MOVE_MODEL, &MM.to_le_bytes());
            g.put(MM, &field::MM_VTABLE.to_le_bytes());
            g.put(MM + field::MM_MODEL, &model.to_le_bytes());
            for (i, v) in [63.0f32, 1.0, 225.0].iter().enumerate() {
                g.put(snap + field::SNAP_POS + 4 * i as u32, &v.to_le_bytes());
            }
            g.put(MGR + MGR_MODEL_SYS, &SYS.to_le_bytes());
            g.put(MM + MM_HANDLER, &H.to_le_bytes());
            g.put(H, &VT.to_le_bytes());
            g.put(VT + 0x18, &TEARDOWN.to_le_bytes());
            g.put(VT + 0x0C, &INIT.to_le_bytes());
            g.put(PLAYER_HEAPS, &LOADER.to_le_bytes());
            g.put(LOADER + 0x2C, &1u32.to_le_bytes());
            let own = g.home(model);
            g.put_param(MM + MM_DRESS, &own);
            g.put_slot(0, &own, 3);
            g.calls.clear();
            g
        }
        /// The game's own outfit in these tests: the gender's default look with skin 3.
        fn home(&mut self, model: u16) -> Vec<u8> {
            let look = Look { skin: 3, ..Look::default_for(if model == MODEL_GIRL { 1 } else { 0 }) };
            self.param_for(&look.appearance().unwrap())
        }
        fn put(&mut self, addr: u32, b: &[u8]) {
            for (i, v) in b.iter().enumerate() {
                self.mem.insert(addr + i as u32, *v);
            }
        }
        fn slot(&self, i: u32) -> u32 {
            SYS + SYS_DRESS_SLOTS + i * DRESS_SLOT_SIZE
        }
        /// The parameter the wardrobe builds for `a` (the player's mark), as bytes.
        fn param_for(&mut self, a: &Appearance) -> Vec<u8> {
            let at = self.scratch() + 0x30;
            dress(self, at, a).unwrap();
            let mut o = self.outfit(at);
            o[3] = PLAYER_MARK;
            self.calls.clear();
            o
        }
        /// A parameter's header and body.
        fn outfit(&mut self, param: u32) -> Vec<u8> {
            param_bytes(self, param).unwrap().to_vec()
        }
        /// A parameter at `at`: the header, its body right after it.
        fn put_param(&mut self, at: u32, o: &[u8]) {
            self.put(at, &o[..8]);
            self.put(at + 8, &(at + 12).to_le_bytes());
            self.put(at + 12, &o[12..]);
        }
        /// Dress-up slot `i` holding the outfit in `state` (its body a heap copy of its own).
        fn put_slot(&mut self, i: u32, o: &[u8], state: u8) {
            let at = self.slot(i);
            let body = 0x0A00_0000 + i * 0x40;
            self.put(at, &[0, state]);
            self.put(at + 4, &o[..8]);
            self.put(at + 12, &body.to_le_bytes());
            self.put(body, &o[12..]);
        }
        fn holding(&mut self, o: &[u8]) -> Option<u32> {
            dress_slot(self, SYS, o.try_into().unwrap()).map(|s| s.0)
        }
        /// Every loading slot finishes.
        fn load(&mut self) {
            for i in 0..DRESS_SLOTS {
                if self.mem.get(&(self.slot(i) + 1)) == Some(&2) {
                    self.put(self.slot(i) + 1, &[3]);
                }
            }
        }
        /// Every unloading slot empties, its player heap freed.
        fn unload(&mut self) {
            for i in 0..DRESS_SLOTS {
                if self.mem.get(&(self.slot(i) + 1)) == Some(&5) {
                    self.put(self.slot(i), &[0; 16]);
                    let heap = self.ours.remove(&i).unwrap();
                    self.put(LOADER + heap, &0u32.to_le_bytes());
                }
            }
        }
        /// Takes free slots for other outfits (the game's) until `left` are free.
        fn fill(&mut self, left: u32) {
            for i in 0..DRESS_SLOTS {
                if free_dress_slots(self, SYS) > left && self.u16(self.slot(i)) == Some(0) {
                    let mut o = vec![i as u8; PARAM_SIZE];
                    o[3] = 1;
                    self.put_slot(i, &o, 3);
                }
            }
        }
        /// The two model heaps, each with one free block of `largest` bytes.
        fn heaps(&mut self, largest: u32) {
            let (a, b) = (0x1400_0100u32, 0x1400_0200u32);
            for (h, next, child, id, free) in [(heap::LINEAR_ROOT, 0u32, a, 1u32, 0u32), (a, b, 0, heap::MODEL_HEAPS[0], 0x1400_0300), (b, 0, 0, heap::MODEL_HEAPS[1], 0x1400_0400)] {
                self.put(h, &0x4558_5048u32.to_le_bytes());
                self.put(h + 8, &next.to_le_bytes());
                self.put(h + 0xC, &child.to_le_bytes());
                self.put(h + 0x24, &free.to_le_bytes());
                self.put(h + 0x50, &id.to_le_bytes());
            }
            for blk in [0x1400_0300u32, 0x1400_0400] {
                self.put(blk, &0x4652u16.to_le_bytes());
                self.put(blk + 4, &largest.to_le_bytes());
                self.put(blk + 0xC, &0u32.to_le_bytes());
            }
        }
        fn count(&self, f: u32) -> usize {
            self.calls.iter().filter(|c| c.0 == f).count()
        }
        /// The outfit the player's +0x998 holds.
        fn worn(&mut self) -> Vec<u8> {
            self.outfit(MM + MM_DRESS)
        }
    }

    fn dll() -> Dll {
        Dll { code: CODE, data: DATA }
    }

    fn boy(hair_colour: u8) -> Appearance {
        Look { hair_colour, ..Look::default_for(0) }.appearance().unwrap()
    }

    /// Ticks (loads and unloads finishing at once) until `seq` is worn; returns the ticks it took.
    fn change(w: &mut Wardrobe, g: &mut Game, seq: u32, a: Appearance) -> u32 {
        w.want(seq, a);
        for n in 1..=400 {
            g.load();
            g.unload();
            assert!(g.ours.len() <= 1, "{} of ours loaded", g.ours.len());
            if let Some(s) = w.tick(g, dll()) {
                assert_eq!(s, seq);
                return n;
            }
        }
        panic!("look {seq} never worn");
    }

    #[test]
    fn asks_with_the_players_mark_then_rebuilds_once_loaded() {
        let mut g = Game::new(MODEL_BOY);
        let mut w = Wardrobe::new();
        let red = boy(1);
        w.want(1, red);
        assert!(!w.between(), "the game's own outfit is still the look worn");
        assert_eq!(w.tick(&mut g, dll()), None);
        assert_eq!(g.count(CODE + REQUEST_DRESS), 1);
        let o = g.param_for(&red);
        let i = g.holding(&o).expect("asked for, in a slot");
        assert_eq!(g.u8(g.slot(i) + 4 + 3), Some(PLAYER_MARK), "header byte 3: the player's part set");
        // loading: no handler call
        for _ in 0..20 {
            assert_eq!(w.tick(&mut g, dll()), None);
        }
        assert!(g.calls.iter().all(|c| ![TEARDOWN, COPY_PARAM, INIT].contains(&c.0)));
        g.load();
        g.calls.clear();
        assert_eq!(w.tick(&mut g, dll()), Some(1));
        let h: Vec<_> = g.calls.iter().filter(|c| [TEARDOWN, COPY_PARAM, INIT].contains(&c.0)).cloned().collect();
        let param = g.scratch() + 0x30;
        assert_eq!(h, vec![(TEARDOWN, vec![H, MM + MM_HANDLER_WORK]), (COPY_PARAM, vec![MM + MM_DRESS, param]), (INIT, vec![H, MM + MM_HANDLER_WORK])]);
        assert_eq!(g.worn(), o, "the player's parameter is the new outfit");
        for _ in 0..200 {
            assert_eq!(w.tick(&mut g, dll()), None, "Some on the rebuild tick only");
        }
        assert_eq!(g.count(TEARDOWN), 1);
    }

    #[test]
    fn a_change_between_two_of_ours_puts_the_games_own_on_first() {
        let mut g = Game::new(MODEL_BOY);
        let mut w = Wardrobe::new();
        let home = g.worn();
        change(&mut w, &mut g, 1, boy(1));
        let first = g.param_for(&boy(1));
        w.want(2, boy(2));
        g.calls.clear();
        assert_eq!(w.tick(&mut g, dll()), None);
        assert_eq!(g.worn(), home, "the game's own outfit back on");
        assert!(w.between());
        assert_eq!(g.count(CODE + REQUEST_DRESS), 0, "nothing asked while ours is loaded");
        for _ in 0..TAKE_OFF - 1 {
            assert_eq!(w.tick(&mut g, dll()), None);
        }
        assert_eq!(g.count(CODE + RELEASE_DRESS), 0, "not before TAKE_OFF ticks");
        w.tick(&mut g, dll());
        assert_eq!(g.count(CODE + RELEASE_DRESS), 1, "released TAKE_OFF ticks after");
        for _ in 0..20 {
            assert_eq!(w.tick(&mut g, dll()), None);
        }
        assert_eq!(g.count(CODE + REQUEST_DRESS), 0, "not asked while ours unloads");
        g.unload();
        assert_eq!(g.holding(&first), None);
        w.tick(&mut g, dll());
        assert_eq!(g.count(CODE + REQUEST_DRESS), 1, "asked once ours unloaded");
        g.load();
        assert_eq!(w.tick(&mut g, dll()), Some(2));
        assert!(!w.between());
        assert_eq!(g.worn(), g.param_for(&boy(2)));
        assert_eq!(g.torn_with, vec![1, 1, 1]);
    }

    #[test]
    fn at_most_one_of_ours_and_the_games_own_is_never_released() {
        let mut g = Game::new(MODEL_BOY);
        let home = g.worn();
        let mut w = Wardrobe::new();
        for seq in 1..=6u32 {
            change(&mut w, &mut g, seq, boy(seq as u8 % HAIR_COLOURS));
        }
        assert_eq!(g.worn(), g.param_for(&boy(1)));
        assert_eq!(g.holding(&home), Some(0), "the game's own outfit stays loaded");
        // its own look: worn at once, from its loaded slot
        let n = g.count(CODE + REQUEST_DRESS);
        let own = Look { skin: 3, ..Look::default_for(0) }.appearance().unwrap();
        assert_eq!(change(&mut w, &mut g, 7, own), 1);
        assert_eq!(g.worn(), home);
        assert_eq!(g.count(CODE + REQUEST_DRESS), n);
        // the look worn, wanted again: its seq at once, no rebuild
        let torn = g.count(TEARDOWN);
        assert_eq!(change(&mut w, &mut g, 8, own), 1);
        assert_eq!(g.count(TEARDOWN), torn);
    }

    /// A look asked for whose slot went before it was worn is asked for again only while a player heap
    /// is free (the fake panics on a load with both taken, as the real loader faults).
    #[test]
    fn asked_again_only_with_a_free_player_heap() {
        let mut g = Game::new(MODEL_BOY);
        let mut w = Wardrobe::new();
        let o = g.param_for(&boy(1));
        w.want(1, boy(1));
        assert_eq!(w.tick(&mut g, dll()), None);
        let i = g.holding(&o).unwrap();
        // its slot went (a field close), and something else holds the free player heap now
        g.put(g.slot(i), &[0; 16]);
        g.ours.clear();
        g.put(LOADER + 0x30, &1u32.to_le_bytes());
        for _ in 0..20 {
            assert_eq!(w.tick(&mut g, dll()), None);
        }
        assert_eq!(g.count(CODE + REQUEST_DRESS), 1, "not asked again with both player heaps taken");
        g.put(LOADER + 0x30, &0u32.to_le_bytes());
        w.tick(&mut g, dll());
        assert_eq!(g.count(CODE + REQUEST_DRESS), 2, "asked again once one is free");
        g.load();
        assert_eq!(w.tick(&mut g, dll()), Some(1));
    }

    #[test]
    fn gated_on_free_slots_and_heaps() {
        let mut g = Game::new(MODEL_BOY);
        let mut w = Wardrobe::new();
        g.fill(DRESS_KEEP);
        w.want(1, boy(1));
        for _ in 0..20 {
            assert_eq!(w.tick(&mut g, dll()), None);
        }
        assert_eq!(g.count(CODE + REQUEST_DRESS), 0, "DRESS_KEEP free slots: nothing asked");
        let taken = g.slot(1);
        g.put(taken, &[0; 16]);
        w.tick(&mut g, dll());
        assert_eq!(g.count(CODE + REQUEST_DRESS), 1, "asked once a slot is free");

        let mut g = Game::new(MODEL_BOY);
        let mut w = Wardrobe::new();
        g.heaps(DRESS_HEAP - 1);
        w.want(1, boy(1));
        for _ in 0..20 {
            assert_eq!(w.tick(&mut g, dll()), None);
        }
        assert_eq!(g.count(CODE + REQUEST_DRESS), 0, "the model heap under DRESS_HEAP: nothing asked");
        g.heaps(DRESS_HEAP);
        w.tick(&mut g, dll());
        assert_eq!(g.count(CODE + REQUEST_DRESS), 1, "asked once the heap has room");

        // both player heaps taken (by something else): the loader would fault, so nothing is asked
        let mut g = Game::new(MODEL_BOY);
        let mut w = Wardrobe::new();
        g.put(LOADER + 0x30, &1u32.to_le_bytes());
        w.want(1, boy(1));
        for _ in 0..20 {
            assert_eq!(w.tick(&mut g, dll()), None);
        }
        assert_eq!(g.count(CODE + REQUEST_DRESS), 0, "no player heap free: nothing asked");
        g.put(LOADER + 0x30, &0u32.to_le_bytes());
        w.tick(&mut g, dll());
        assert_eq!(g.count(CODE + REQUEST_DRESS), 1, "asked once a player heap is free");
    }

    #[test]
    fn the_newest_want_wins() {
        let mut g = Game::new(MODEL_BOY);
        let mut w = Wardrobe::new();
        w.want(1, boy(1));
        w.want(2, boy(2));
        assert_eq!(change(&mut w, &mut g, 2, boy(2)), 2);
        for _ in 0..300 {
            g.load();
            assert_eq!(w.tick(&mut g, dll()), None);
        }
        assert_eq!(g.count(CODE + REQUEST_DRESS), 1, "only 2 asked for");
        assert_eq!(g.worn(), g.param_for(&boy(2)));
    }

    #[test]
    fn a_bad_look_or_the_other_gender_is_never_dressed() {
        let girl = Look::default_for(1).appearance().unwrap();
        let bad = Appearance { gender: 0, fashion: [0xFF; 16] };
        assert!(!bad.validate());
        for (model, a) in [(MODEL_BOY, bad), (MODEL_BOY, girl), (MODEL_GIRL, boy(1))] {
            let mut g = Game::new(model);
            let mut w = Wardrobe::new();
            w.want(1, a);
            for _ in 0..100 {
                g.load();
                assert_eq!(w.tick(&mut g, dll()), None);
            }
            assert!(g.calls.is_empty(), "model {model}: {:?}", g.calls);
            assert!(!w.between());
        }
        // the girl's own model takes a girl's look
        let mut g = Game::new(MODEL_GIRL);
        let mut w = Wardrobe::new();
        change(&mut w, &mut g, 1, Look { hair_colour: 2, ..Look::default_for(1) }.appearance().unwrap());
        assert!(g.count(DRESS_GIRL) >= 1);
    }

    /// Ours worn, B wanted: the game's own goes on (between). Then a want that needs no load must end `between`, or
    /// the preview never sends a frame again.
    fn between_b(g: &mut Game, w: &mut Wardrobe) {
        change(w, g, 1, boy(1));
        w.want(2, boy(2));
        assert_eq!(w.tick(g, dll()), None);
        assert!(w.between(), "the game's own outfit on, on the way to B");
    }

    #[test]
    fn the_games_own_look_wanted_between_two_looks_ends_between() {
        let mut g = Game::new(MODEL_BOY);
        let home = g.worn();
        let mut w = Wardrobe::new();
        between_b(&mut g, &mut w);
        let own = Look { skin: 3, ..Look::default_for(0) }.appearance().unwrap();
        w.want(3, own);
        assert_eq!(w.tick(&mut g, dll()), Some(3), "the game's own look is on already");
        assert!(!w.between());
        assert_eq!(g.worn(), home);
        for _ in 0..100 {
            g.load();
            g.unload();
            assert_eq!(w.tick(&mut g, dll()), None, "B was replaced: nothing more is worn");
        }
        assert!(!w.between());
        assert_eq!(g.worn(), home);
    }

    #[test]
    fn a_look_for_the_other_gender_between_two_looks_ends_between() {
        let mut g = Game::new(MODEL_BOY);
        let mut w = Wardrobe::new();
        between_b(&mut g, &mut w);
        w.want(3, Look::default_for(1).appearance().unwrap());
        assert_eq!(w.tick(&mut g, dll()), None, "dropped");
        assert!(!w.between(), "nothing is on the way: the game's own outfit is the look worn");
        // the next look still comes
        assert_eq!(change(&mut w, &mut g, 4, boy(3)) > 0, true);
        assert!(!w.between());
    }
}
