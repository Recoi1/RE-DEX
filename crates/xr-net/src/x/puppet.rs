//! Remote players as puppets: NPCs in X's own move-model pool. Layouts and evidence:
//! docs/re/x-field-move-models.md, "Puppets".
//!
//! The runtime calls [`Puppets::tick`] once per pass of the game's main loop and [`Puppets::sweep`] right before the
//! field save, both from hooks on the game's main thread (between procs, so nothing is half way through the pool),
//! with a [`Calls`] that runs game functions there. Every change to a puppet goes through the game's own functions
//! (Create, DeleteModel, SetPosition, SetDir, SetStatusBit), except status 2's "not saved" bit, which has no setter.
//!
//! The movement follows eidos's field avatars (sdk/mod_avatar.c, MIT; see NOTICE): walk toward the newest place the
//! other player sent at their mode's speed, catch up by a sixth of the lag past one tile, jump there past two tiles,
//! and face the way they face.
use super::field::{self, Appearance, Player};
use super::wardrobe::PLAYER_MARK;
use crate::Guest;

/// Where DllField is loaded: its code segment (CRO segment 0; `DllField+X` offsets are from it) and its data segment
/// (segment 2: the player snapshot).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Dll {
    pub code: u32,
    pub data: u32,
}

/// `MoveModel* Create(MoveModelManager*, const ZoneNpc* header, u16 zone, u32 extra)`; hangs the game when the pool is
/// full, so it is called only with free slots to spare.
pub const CREATE: u32 = 0x003D_41D8;
/// `DeleteModel(MoveModelManager*, MoveModel*)`: Delete (0x3D9A9C), then releases the model's resource unless
/// another move model uses it (the tail at 0x3D585C), as DeleteAll does for each model.
pub const DELETE: u32 = 0x003D_5818;
/// `SetTransform(MoveModel*, const Transform*)`: copies the 0x2C-byte transform (a word, the position at +4, rotation,
/// scale) to the model's +0xE8 and, once the model is set up, re-syncs the drawn and collision position from it. (The
/// other setter, code.bin 0x3D07F8, takes the field's own position object, whose first word points into the map.)
pub const SET_POS: u32 = 0x003D_0748;
/// The model's transform block: +0xE8, 0x2C bytes; the world position is its +4 (`field::MM_POS`).
const TRANSFORM: u32 = 0xE8;
const TRANSFORM_SIZE: usize = 0x2C;
/// `SetDir(MoveModel*, u32 dir)`: 0 up, 1 down, 2 left, 3 right (what Create does with the header's facing).
pub const SET_DIR: u32 = 0x003D_33D4;
/// `SetMotion(MoveModel*, u32 motion, u32 dir)`: plays a motion facing `dir`. The player's step
/// (DllField+0x705F4..0x70624) passes the same codes to it.
pub const SET_MOTION: u32 = 0x003D_6FF0;
/// Motions: action commands (codes below 0xD4, DllField text+0x49428 sets them). Walk (12) and run (13) are the
/// player's one-tile steps and move the model themselves, so puppets use the in-place ones, found by a scan
/// of the codes (docs/re/x-field-move-models.md, "Motions"): 25 to 28 step in place for 15, 7, 3 and 1 field updates
/// (+1); 26 lasts as long as a walk step (12: 8 updates), 27 as a run step. 8 and 9 hang the game, 50 hides the model.
pub const MOTION_STAND: u32 = 0;
pub const MOTION_WALK: u32 = 12;
pub const MOTION_RUN: u32 = 13;
pub const MOTION_WALK_IN_PLACE: u32 = 26;
pub const MOTION_RUN_IN_PLACE: u32 = 27;
/// The move model's action state: 1 while an action plays, 2 once it ended.
const MM_ACTION_STATE: u32 = 0x8FC;
/// SetMotion's own directions are 0 down, 1 up, 2 left, 3 right (code.bin 0x542F14 flips them to the model's): by
/// puppet facing (0 up, 1 down, 2 left, 3 right).
const MOTION_DIR: [u32; 4] = [1, 0, 2, 3];
/// `SetStatusBit(MoveModel*, u32 mask)`: status |= mask.
pub const SET_STATUS: u32 = 0x003D_037C;

/// Status bit: set up (code.bin 0x3D7800 sets it on the model's first update, Delete clears it).
pub const ACTIVE: u32 = 0x2;
/// Status bit: the hit checks (code.bin 0x3D2534 and 0x4A6A84, through the pool search 0x4AA98C with flag 2) skip
/// the model, so it blocks nobody and nobody blocks it.
pub const NO_HIT: u32 = 0x100;
/// The zone NPC header's script id that runs nothing (the game's own: the zone data's hidden entries carry it, and
/// code.bin 0x3D5A44 writes it into a slot it empties).
pub const NO_SCRIPT: u16 = 2000;
/// Puppets' object ids: 0x8000 and up, outside the zone's (its event data) and the player's 0xFF.
pub const ID_BASE: u16 = 0x8000;
/// The player models (DllField+0x1C8EC picks 2 for a boy, 1 for a girl).
pub const MODEL_BOY: u16 = 2;
pub const MODEL_GIRL: u16 = 1;
/// Free pool slots left to the game: its scripts and zone loads add NPCs, and a full pool hangs it.
pub const MIN_FREE: usize = 6;
/// Puppets at most. Each one's drawing allocates from the field's heap (code.bin 0x3975C4 -> the allocator 0x12B0C8),
/// and an allocation that fails ends in the fatal loop: 17 puppets in the Laverre Pokemon Center hung the game there.
/// shortcut: a fixed cap; upgrade to asking the heap for its free size once its method is found.
pub const MAX_PUPPETS: usize = 8;
/// With more players than MAX_PUPPETS, a shown puppet counts this much nearer than it is when the nearest are picked,
/// so two players at about the same distance do not swap in and out (each swap is a Delete and a Create).
pub const KEEP_MARGIN: f32 = 3.0 * TILE;
/// Ticks the map must have stayed the same before puppets appear: a map change empties the pool and refills it.
pub const SETTLE: u32 = 30;
/// Ticks after the zone NPC loader (code.bin 0x3D6DF0, hooked) ran for this map before puppets appear, so they
/// allocate after the zone's own NPCs.
pub const AFTER_LOAD: u32 = 15;
/// A loader run counts for a map only if it came at most this many ticks before the map's first tick (the loader runs
/// during the map change, while the snapshot is gone or still shows the old map); an older one is from an earlier visit.
pub const LOAD_LEAD: u32 = 120;
/// Without a loader run seen for the map (DllField's own call to the loader is not hooked), this long instead.
pub const SETTLE_NO_LOADER: u32 = 150;
/// Ticks between two "others are nearer" swaps (about a second): each is a Delete and a Create on the heap.
pub const SWAP_GAP: u32 = 30;
/// A remote position further than this from the local player (x or z) is pulled in to it (and never below 0, where no
/// map is).
pub const MAX_REACH: f32 = 32.0 * TILE;
/// A remote height further than this from the local player's is pulled in to it.
pub const MAX_RISE: f32 = 4.0 * TILE;
/// A puppet takes about 71 KB of the field's model heaps (heap::MODEL_HEAPS). It is spawned only while each of them
/// has a free block of PUPPET_HEAP + HEAP_KEEP (which heap a puppet's blocks come from is not known, and a sum of both
/// could hide one that is nearly full).
pub const PUPPET_HEAP: u32 = 80 * 1024;
pub const HEAP_KEEP: u32 = 128 * 1024;
/// Ticks (frames) a session waits after its puppet was deleted before it is spawned again: a server that announces
/// and drops a player over and over costs a Delete and a Create a second at most.
pub const RESPAWN_GAP: u32 = 60;
/// Ticks between two spawns.
pub const SPAWN_GAP: u32 = 4;
/// Ticks an outfit xr-net asked for goes unworn before it is released. Released in the tick that deleted its last
/// puppet, the game ended in its heap error loop (code.bin 0x12B2B4): the deleted model still uses the parts until
/// its teardown is done.
pub const RELEASE_AFTER: u32 = 60;

/// Frames without moving before a puppet stands (a position arrives every 6 frames).
const STAND_AFTER: u32 = 8;

/// One tile in world units.
pub const TILE: f32 = 18.0;
/// eidos: past one tile behind, catch up by a sixth of the lag a tick; past two, jump.
const CATCH_UP: f32 = TILE;
const SNAP: f32 = 2.0 * TILE;

/// An outfit xr-net asked for: its dress-up parameter's bytes (header with the body pointer zeroed, then the body; the
/// slots are matched by them) and the ticks no puppet has worn it.
#[derive(Debug, Clone, PartialEq)]
struct Requested {
    look: Appearance,
    param: [u8; PARAM_SIZE],
    unworn: u32,
}

/// What a remote player on our map looks like now: their newest state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Target {
    pub pos: [f32; 3],
    /// 0 up, 1 down, 2 left, 3 right
    pub facing: u8,
    /// `redex_proto::MODE_*`
    pub mode: u8,
    /// 0 boy, 1 girl: the model, and the plain look's when `look` is None
    pub gender: u8,
    /// their outfit, validated (`Appearance::from_bytes`); None: the plain look
    pub look: Option<Appearance>,
    /// Who is shown first when more than MAX_PUPPETS players are on the map: a higher rank, then the nearest. 0 for
    /// everyone for now; friends and team members get more.
    pub rank: u8,
}

/// Runs game code. `call` runs `f` with up to four arguments on the game's main thread and returns r0. `scratch` is
/// 0x100 bytes of guest memory that stay valid while the hook runs (arguments passed by pointer go there: a zone header
/// at +0, a dress-up parameter at +0x30).
pub trait Calls: Guest {
    fn write(&mut self, addr: u32, data: &[u8]) -> bool;
    fn call(&mut self, f: u32, args: &[u32]) -> u32;
    fn scratch(&self) -> u32;
}

#[derive(Debug, Clone, PartialEq)]
struct Puppet {
    session: u32,
    mm: u32,
    id: u16,
    zone: u16,
    pos: [f32; 3],
    facing: u8,
    /// the motion last played and its direction
    motion: Option<(u32, u8)>,
    /// frames since it last moved
    still: u32,
    /// its last step (x, z), for Motion's reversals
    last_step: [f32; 2],
    /// the target's look when it was made (a new one is a swap)
    look: Option<Appearance>,
    /// the outfit it was made with (Create's `extra`); None: the plain look
    worn: Option<Appearance>,
    /// the player model it was made with (a stand-in model when its gender's was not loaded)
    model: u16,
}

/// How the puppets moved (smoothness under jitter): steps taken, the longest step in a tick (world
/// units; walking is TILE/14), jumps (steps over two tiles: the snap) and reversals (a step against the one before it,
/// faster than walking: a rubber band; a lagging puppet cutting a corner turns back at walking speed or slower).
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Motion {
    pub steps: u32,
    pub max_step: f32,
    pub jumps: u32,
    pub reversals: u32,
}

impl Motion {
    fn record(&mut self, pup: &mut Puppet, before: [f32; 3]) {
        let d = [pup.pos[0] - before[0], pup.pos[2] - before[2]];
        if d == [0.0, 0.0] {
            return;
        }
        let len = d[0].abs().max(d[1].abs());
        self.steps += 1;
        self.max_step = self.max_step.max(len);
        self.jumps += u32::from(len > SNAP);
        self.reversals += u32::from(d[0] * pup.last_step[0] + d[1] * pup.last_step[1] < 0.0 && len > speed(redex_proto::MODE_WALK) + 0.01);
        pup.last_step = d;
    }
}

/// The puppets and what they should show. The runtime's main loop calls [`Puppets::want`] after each frame; the
/// hooks do the rest.
#[derive(Debug, Default)]
pub struct Puppets {
    /// (map, the players on it by session), from the main loop
    wanted: Option<(u16, Vec<(u32, Target)>)>,
    live: Vec<Puppet>,
    /// the map the player stood on last tick, and for how many ticks
    map: Option<(u16, u32)>,
    next_id: u16,
    /// the manager the puppets live in (a new one is a new pool: nothing of ours is known in it)
    mgr: Option<u32>,
    /// the zone the hooked loader last ran for, and ticks since
    loaded: Option<(u16, u32)>,
    /// ticks until the next nearest-8 swap may happen
    swap_wait: u32,
    /// ticks until the next spawn may happen
    spawn_wait: u32,
    /// sessions whose puppet was deleted, and the ticks until they may come back (RESPAWN_GAP)
    cooling: Vec<(u32, u32)>,
    /// the model heaps could not be read (logged once)
    heap_unknown: bool,
    /// outfits whose dress-up parts xr-net asked for (RequestDressUp) in the model system `.0`; each is released
    /// RELEASE_AFTER ticks after the last puppet wearing it went
    requested: (u32, Vec<Requested>),
    /// ticks the outfit loader has been busy (dressed spawns wait for it, at most DRESS_WAIT), and whether a wait was
    /// logged since it was last idle
    dress_wait: u32,
    wait_said: bool,
    /// worn outfits asked for again since their slot went (logged once each)
    asked_again: Vec<Appearance>,
    /// a girl with no outfit was not made (logged once)
    ghost_said: bool,
    /// players not shown because the pool had no room (logged once per map)
    pub hidden: usize,
    /// what changed, for the log ("spawn ...", "delete ...")
    pub events: Vec<String>,
    /// how they moved, over the whole run
    pub motion: Motion,
}

impl Puppets {
    pub fn new() -> Puppets {
        Puppets::default()
    }

    /// The remote players on `map` (the local player's map; others are dropped). Called once per frame.
    pub fn want(&mut self, map: u16, players: Vec<(u32, Target)>) {
        self.wanted = Some((map, players));
    }

    /// `want`, beside the players already wanted on `map` (a test feed next to a real world; the same session twice is
    /// the newest).
    pub fn want_also(&mut self, map: u16, players: Vec<(u32, Target)>) {
        match &mut self.wanted {
            Some((m, w)) if *m == map => {
                w.retain(|(s, _)| !players.iter().any(|p| p.0 == *s));
                w.extend(players);
            }
            _ => self.wanted = Some((map, players)),
        }
    }

    /// The live puppets: (session, move model, position).
    pub fn shown(&self) -> Vec<(u32, u32, [f32; 3])> {
        self.live.iter().map(|p| (p.session, p.mm, p.pos)).collect()
    }

    /// The live puppets the game draws now: (session, position). A dressed one is drawn only once a dress-up slot
    /// holds its outfit, loaded (state 3); until then (or with no field) it is left out, so a name never floats over
    /// nothing.
    pub fn drawn(&self, g: &mut impl Guest) -> Vec<(u32, [f32; 3])> {
        let sys = self.mgr.and_then(|m| g.u32(m + MGR_MODEL_SYS));
        let loaded = |g: &mut _, pup: &Puppet| match pup.worn {
            None => true,
            Some(look) => {
                let r = self.requested.1.iter().find(|r| r.look == look);
                matches!((sys, r), (Some(sys), Some(r)) if dress_slot(g, sys, &r.param).is_some_and(|(_, _, state)| state == 3))
            }
        };
        self.live.iter().filter(|p| loaded(g, p)).map(|p| (p.session, p.pos)).collect()
    }

    /// Before the field save: every puppet goes (the game would skip them anyway, status 2's NO_SAVE; this keeps
    /// even a puppet that lost the bit out). The next tick puts them back.
    pub fn sweep(&mut self, g: &mut impl Calls, dll: Dll) {
        // no field: nothing can be deleted now; the next tick with a field deletes whatever is left (orphans)
        // (and releases their outfits)
        let Some(p) = field::player(g, dll.data).filter(|p| field::slots(g, p.manager).is_some()) else {
            self.map = None;
            return;
        };
        for pup in std::mem::take(&mut self.live) {
            self.delete(g, p.manager, &pup, "save");
        }
        self.reap(g, p.manager);
        // they reappear only once the save is over and the map has settled again (the same map: its load still counts only if the save came
        // within LOAD_LEAD ticks of it; else the SETTLE_NO_LOADER fallback)
        self.map = self.map.map(|(m, _)| (m, 0));
    }

    /// Once per main-loop pass. `data` is DllField's data segment (None: the field is not loaded, and with it went
    /// the pool).
    pub fn tick(&mut self, g: &mut impl Calls, dll: Option<Dll>) {
        self.loaded = self.loaded.map(|(z, n)| (z, n.saturating_add(1)));
        self.swap_wait = self.swap_wait.saturating_sub(1);
        self.spawn_wait = self.spawn_wait.saturating_sub(1);
        self.cooling.retain_mut(|(_, n)| {
            *n -= 1;
            *n > 0
        });
        // the field and its whole pool, checked every tick (a battle unloads DllField; a new manager is a new pool)
        let p = dll.and_then(|d| Some((field::player(g, d.data)?, d))).filter(|(p, _)| field::slots(g, p.manager).is_some());
        let Some((p, dll)) = p else {
            // nothing is touched without the field (a battle, an event that drops the player's controller for a few
            // ticks); the puppets stay known, and the next tick with the field checks each slot again (a slot the
            // game took is forgotten, one with a puppet id nobody knows is deleted)
            self.map = None;
            return;
        };
        if self.mgr != Some(p.manager) {
            // the old pool's puppets are not ours to touch any more
            if !self.live.is_empty() {
                self.events.push(format!("{} puppets forgotten: a new move-model manager", self.live.len()));
            }
            self.live.clear();
            self.map = None;
            self.mgr = Some(p.manager);
        }
        // the outfits asked for in another model system are not ours to release
        let sys = g.u32(p.manager + MGR_MODEL_SYS).unwrap_or(0);
        if self.requested.0 != sys {
            self.requested = (sys, Vec::new());
        }
        self.map = match self.map {
            Some((m, n)) if m == p.map => Some((m, n.saturating_add(1))),
            _ => Some((p.map, 0)),
        };
        // a slot the game emptied or reused (map change, DeleteAll) is no longer ours: forget it, never touch it
        let n = self.live.len();
        self.live.retain(|pup| owned(g, pup));
        if self.live.len() != n {
            self.events.push(format!("{} puppets the game took (their slots were emptied or reused)", n - self.live.len()));
        }
        // ours but not known (left behind across a battle or a lost field): only puppets carry ids from 0x8000
        self.reap(g, p.manager);
        // a surfing player is not shown until surfing is traced (no wrong pose on land or water): as if gone
        let wanted: Vec<(u32, Target)> = match &self.wanted {
            Some((m, w)) if *m == p.map => w.iter().filter(|(_, t)| t.mode != redex_proto::MODE_SURF).filter_map(|(s, t)| Some((*s, near(t, p.pos)?))).collect(),
            _ => Vec::new(),
        };
        let chosen = self.pick(&wanted, p.pos);
        for pup in std::mem::take(&mut self.live) {
            let t = wanted.iter().find(|w| w.0 == pup.session).map(|w| w.1);
            let why = if pup.zone != p.map {
                "map change"
            } else if t.is_none() {
                "gone"
            } else if t.is_some_and(|t| t.look != pup.look || (player_model(t.gender) != pup.model && resident(g, p.manager, player_model(t.gender)))) {
                // a new outfit is a new model (Create takes it): made again under the spawn gap and the heap gate
                "outfit change"
            } else if !chosen.contains(&pup.session) && self.swap_wait == 0 {
                self.swap_wait = SWAP_GAP;
                "others are nearer"
            } else {
                self.live.push(pup);
                continue;
            };
            // never a Delete over parts that are gone or going (after a battle): the outfit is asked for again below,
            // and the Delete waits until a slot holds it again
            if self.parts_missing(g, sys, &pup) {
                self.live.push(pup);
                continue;
            }
            self.delete(g, p.manager, &pup, why);
            self.cooling.push((pup.session, RESPAWN_GAP));
        }
        self.ask_again(g, dll, sys);
        let stable = self.map.map_or(0, |(_, n)| n);
        // the run must be for this map and from this visit: at most LOAD_LEAD ticks before the map's first tick (an
        // A -> B -> A trip without a hooked load must not reuse A's old run; the snapshot can show the old map for a
        // tick after the load, so the run is not cleared at a map change)
        let loaded = self.loaded.is_some_and(|(z, n)| z == p.map && n >= AFTER_LOAD && n <= stable.saturating_add(LOAD_LEAD));
        let settled = stable >= SETTLE && (loaded || stable >= SETTLE_NO_LOADER);
        let mut hidden = 0;
        // dressed spawns wait while the outfit loader works (each load takes model heap: one at a time, so the heap
        // gate sees it), but not for ever: past DRESS_WAIT they come plain
        let loading = dress_busy(g, sys);
        self.dress_wait = if loading { self.dress_wait.saturating_add(1) } else { 0 };
        self.wait_said &= loading;
        let waited_out = self.dress_wait >= DRESS_WAIT;
        if self.dress_wait == DRESS_WAIT {
            self.events.push(format!("the outfit loader has been busy for {DRESS_WAIT} ticks: dressed players come plain until it is done"));
        }
        for (session, t) in &wanted {
            match self.live.iter().position(|pup| pup.session == *session) {
                Some(i) => {
                    let mut pup = self.live[i].clone();
                    let before = pup.pos;
                    step(g, &mut pup, t);
                    self.motion.record(&mut pup, before);
                    self.live[i] = pup;
                }
                // one spawn per SPAWN_GAP ticks, and none for a session whose puppet just went (neither counts as hidden)
                None if settled && chosen.contains(session) && (self.spawn_wait > 0 || self.cooling.iter().any(|c| c.0 == *session)) => {}
                None if settled && chosen.contains(session) && t.look.is_some() && loading && !waited_out => {
                    if !std::mem::replace(&mut self.wait_said, true) {
                        self.events.push(format!("the spawn of session {session} waits for the outfit loader"));
                    }
                }
                None if settled && chosen.contains(session) => {
                    if self.spawn(g, dll, &p, *session, t, loading) {
                        self.spawn_wait = SPAWN_GAP;
                    } else {
                        hidden += 1;
                    }
                }
                None => hidden += usize::from(settled),
            }
        }
        self.release_unworn(g, dll, p.manager, sys);
        if hidden != self.hidden && hidden > 0 {
            self.events.push(format!("{hidden} players not shown: at most {MAX_PUPPETS} puppets, and {MIN_FREE} free slots kept for the game"));
        }
        self.hidden = hidden;
    }

    /// The hooked zone NPC loader ran for `zone` (before the game's Create calls): puppets wait AFTER_LOAD ticks.
    pub fn zone_loaded(&mut self, zone: u16) {
        self.events.push(format!("the zone NPC loader ran for zone {zone} (the player's map was {:?})", self.map.map(|m| m.0)));
        self.loaded = Some((zone, 0));
    }

    /// Deletes every slot in use that carries the puppet marks (`owned`) and that `live` does not know.
    fn reap(&mut self, g: &mut impl Calls, mgr: u32) {
        let Some(slots) = field::slots(g, mgr) else { return };
        let known: Vec<u32> = self.live.iter().map(|pup| pup.mm).collect();
        for s in slots.iter().filter(|s| s.id >= ID_BASE && !known.contains(&s.addr)) {
            let orphan = Puppet { session: 0, mm: s.addr, id: s.id, zone: s.zone, pos: [0.0; 3], facing: 0, motion: None, still: 0, last_step: [0.0; 2], look: None, worn: None, model: 0 };
            self.delete(g, mgr, &orphan, "orphan");
        }
    }

    /// The sessions to show: at most MAX_PUPPETS, by rank, then distance to the local player at `me` (x and z),
    /// shown ones counting KEEP_MARGIN nearer.
    fn pick(&self, wanted: &[(u32, Target)], me: [f32; 3]) -> Vec<u32> {
        let mut by: Vec<(u8, f32, u32)> = wanted
            .iter()
            .map(|(s, t)| {
                let d = (t.pos[0] - me[0]).hypot(t.pos[2] - me[2]);
                let shown = self.live.iter().any(|pup| pup.session == *s);
                (t.rank, if shown { d - KEEP_MARGIN } else { d }, *s)
            })
            .collect();
        by.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.total_cmp(&b.1)).then(a.2.cmp(&b.2)));
        by.into_iter().take(MAX_PUPPETS).map(|(_, _, s)| s).collect()
    }

    /// The game is about to Create a move model in `mgr`'s pool: when no slot is free, the newest puppet goes, so the
    /// game's own NPC always fits (the next tick shows the puppet again only with MIN_FREE slots to spare).
    pub fn make_room(&mut self, g: &mut impl Calls, dll: Dll, mgr: u32) {
        // the player's manager, or (no snapshot: a scripted event) the last one the puppets lived in; either way it must
        // be this Create's and a whole pool of 32 move models
        let ours = field::player(g, dll.data).map(|p| p.manager).or(self.mgr).filter(|&m| m == mgr);
        if ours.is_none() || field::slots(g, mgr).is_none() {
            return;
        }
        // until a slot is really free (a puppet the game already took frees nothing)
        while field::slots(g, mgr).is_some_and(|s| s.len() >= usize::from(field::POOL_SLOTS)) {
            // a puppet whose outfit's parts are gone or going is never deleted (a Delete over freed parts): the newest one
            // with its parts goes, and only when every one is such a puppet, the newest
            let sys = g.u32(mgr + MGR_MODEL_SYS).unwrap_or(0);
            let at = self.live.iter().rposition(|p| !self.parts_missing(g, sys, p)).or(self.live.len().checked_sub(1));
            let Some(pup) = at.map(|i| self.live.remove(i)) else { return };
            self.delete(g, mgr, &pup, "the game needs the slot");
            self.cooling.push((pup.session, RESPAWN_GAP));
        }
    }

    /// `plain`: the outfit loader is stuck (DRESS_WAIT): no outfit this time.
    fn spawn(&mut self, g: &mut impl Calls, dll: Dll, p: &Player, session: u32, t: &Target, plain: bool) -> bool {
        let Some(used) = field::slots(g, p.manager).map(|s| s.len()) else { return false };
        if self.live.len() >= MAX_PUPPETS || usize::from(field::POOL_SLOTS) - used <= MIN_FREE {
            return false;
        }
        // each of the field's model heaps has a free block for this puppet and HEAP_KEEP (not found: the cap alone);
        // an outfit's parts need DRESS_HEAP more, else the puppet wears the plain look
        let heaps = super::heap::free(g, &super::heap::MODEL_HEAPS);
        let room = |need: u32| heaps.as_ref().is_none_or(|f| f.iter().all(|f| f.largest >= need + HEAP_KEEP));
        if !room(PUPPET_HEAP) {
            return false;
        }
        if heaps.is_none() && !self.heap_unknown {
            self.heap_unknown = true;
            self.events.push("the model heaps were not found: only the cap limits puppets".into());
        }
        // 0x8000..=0xFFFE (0xFFFF is no id)
        let id = ID_BASE + self.next_id % 0x7FFF;
        self.next_id = (self.next_id + 1) % 0x7FFF;
        // a model whose resource is not loaded hangs Create (code.bin 0x3D4E78 ends in the fatal loop when the map
        // does not load models on demand): the other gender's model stands in. Never the local player's own model id
        // (another form's model would fail `owned`, and the puppet could never be deleted)
        let (want, other) = (player_model(t.gender), player_model(1 - t.gender.min(1)));
        let model = if resident(g, p.manager, want) { want } else { other };
        if !resident(g, p.manager, model) {
            self.events.push(format!("spawn of session {session} skipped: model {model} is not loaded"));
            return false;
        }
        let hdr = header(id, model, t);
        let at = g.scratch();
        if !g.write(at, &hdr) {
            return false;
        }
        // the outfit: only on its own gender's model (a stand-in model gets the plain look), with a free dress-up slot
        // (DRESS_KEEP left to the game) and the heap for its parts.
        // shortcut: a puppet made plain here stays plain until its look changes; upgrade: remake it once a slot and the
        // heap are free again
        let sys = g.u32(p.manager + MGR_MODEL_SYS).unwrap_or(0);
        let worn = t.look.filter(|a| !plain && player_model(a.gender) == model && free_dress_slots(g, sys) > DRESS_KEEP && room(PUPPET_HEAP + DRESS_HEAP));
        // the parameter after the header (DRESS_SIZE bytes; the callees' stacks grow down from `at`)
        let param = match worn {
            Some(a) => match dress(g, at + 0x30, &a) {
                Some(param) => param,
                None => return false,
            },
            None => 0,
        };
        // a girl's outfit on the player's part set (PLAYER_MARK) takes one of the game's two player heaps, and the local
        // player's outfit holds the other. The part set is picked per look: one of ours already asked for keeps its
        // own; a new one takes the free heap, else (no free heap, or the game's own slot holds this outfit: a twin,
        // whose release would take her parts) the NPC set, where her long hair is a plank but she is seen
        if let Some(a) = worn.filter(|a| a.gender == 1) {
            let bytes = param_bytes(g, param).unwrap_or([0; PARAM_SIZE]);
            let mark = match self.requested.1.iter().find(|r| r.look == a) {
                // its parts are being asked for again (after a field close): a request now could fault the loader
                Some(r) if missing(g, sys, &r.param) => return false,
                Some(r) => r.param[3],
                None if super::wardrobe::player_heaps_full(g) || dress_slot(g, sys, &bytes).is_some() => PUPPET_MARK,
                None => PLAYER_MARK,
            };
            if !g.write(param + 3, &[mark]) {
                return false;
            }
        }
        // the girl model has no body of its own (it is all dress-up parts): without an outfit only her shadow, the
        // held item and the name would show, so she is not made (a stuck loader, no slot or heap, a stand-in model)
        if worn.is_none() && model == MODEL_GIRL {
            if !std::mem::replace(&mut self.ghost_said, true) {
                self.events.push(format!("spawn of session {session} skipped: a girl model with no outfit draws nothing (logged once)"));
            }
            return false;
        }
        // a puppet's parameter carries PUPPET_MARK, so its slot is never one the game holds (the local player's outfit
        // too gets a slot of its own): xr-net alone asks for it and releases it
        if let Some(a) = worn {
            let bytes = param_bytes(g, param).unwrap_or([0; PARAM_SIZE]);
            g.call(dll.code + REQUEST_DRESS, &[sys, param]);
            if !self.requested.1.iter().any(|r| r.look == a) {
                self.requested.1.push(Requested { look: a, param: bytes, unworn: 0 });
            }
        }
        let mm = g.call(CREATE, &[p.manager, at, u32::from(p.map), param]);
        // Create returns a pool slot; anything else is a loud failure, not a write through a bad pointer
        if g.u32(mm) != Some(field::MM_VTABLE) || g.u16(mm + field::MM_ID) != Some(id) {
            self.events.push(format!("spawn of session {session} failed: Create returned 0x{mm:08X}"));
            return false;
        }
        g.call(SET_STATUS, &[mm, NO_HIT]);
        unsaved(g, mm);
        // the header put it on the tile; it moves once the game has set it up (its first update)
        let pup = Puppet { session, mm, id, zone: p.map, pos: t.pos, facing: t.facing, motion: None, still: STAND_AFTER, last_step: [0.0; 2], look: t.look, worn, model };
        let look = match (worn, t.look) {
            (Some(a), _) => format!("dressed {}", hex(&a)),
            (None, Some(_)) => "plain (its outfit did not fit: a stand-in model, no free dress-up slot, no heap or a stuck loader)".into(),
            (None, None) => "plain".into(),
        };
        self.events.push(format!("spawn session {session} as 0x{id:04X} (model {model}) {look} at 0x{mm:08X}, {} slots used", used + 1));
        self.live.push(pup);
        true
    }

    /// A worn outfit whose slot is unloading or gone is asked for again; the puppet is never deleted for it. Only the
    /// game's ReleaseAll at a field close (a battle) does that to a puppet's slot, and the game takes every model's draw
    /// binding down before it (code.bin 0x3D6818-0x3D6854) and binds it again only once the field is back (0x3D4138),
    /// when the dressed draw waits for a loaded slot. A Delete then would run over a model whose parts are gone; asking
    /// again gives it its parts back, and it is drawn once they are in.
    fn parts_missing(&self, g: &mut impl Guest, sys: u32, pup: &Puppet) -> bool {
        let r = self.requested.1.iter().find(|r| pup.worn == Some(r.look));
        r.is_some_and(|r| missing(g, sys, &r.param))
    }

    fn ask_again(&mut self, g: &mut impl Calls, dll: Dll, sys: u32) {
        let mut worn: Vec<Appearance> = Vec::new();
        for a in self.live.iter().filter_map(|pup| pup.worn) {
            if !worn.contains(&a) {
                worn.push(a);
            }
        }
        for a in worn {
            let Some(r) = self.requested.1.iter().find(|r| r.look == a) else { continue };
            if !missing(g, sys, &r.param) {
                self.asked_again.retain(|b| *b != a);
                continue;
            }
            // on the part set it was asked for on; the player's set waits for a free player heap (the loader faults
            // without one)
            let mark = r.param[3];
            if mark == PLAYER_MARK && super::wardrobe::player_heaps_full(g) {
                continue;
            }
            if let Some(param) = dress_as(g, g.scratch() + 0x30, &a, mark) {
                g.call(dll.code + REQUEST_DRESS, &[sys, param]);
                if !self.asked_again.contains(&a) {
                    self.asked_again.push(a);
                    self.events.push(format!("asked again for the outfit {}: its slot was unloading or gone (a field close); no Delete over its parts", hex(&a)));
                }
            }
        }
    }

    /// Releases the outfits xr-net asked for that no puppet has worn for RELEASE_AFTER ticks, by the slot's own
    /// state: a slot that is still loading waits; one the game asked for since (a request xr-net did not make) or that
    /// one of the game's own models wears is left to the game.
    fn release_unworn(&mut self, g: &mut impl Calls, dll: Dll, mgr: u32, sys: u32) {
        for r in &mut self.requested.1 {
            r.unworn = if self.live.iter().any(|pup| pup.worn == Some(r.look)) { 0 } else { r.unworn.saturating_add(1) };
        }
        for r in self.requested.1.clone().into_iter().filter(|r| r.unworn >= RELEASE_AFTER) {
            let slot = dress_slot(g, sys, &r.param);
            if slot.is_some_and(|(_, request, state)| request == 0 && matches!(state, 1 | 2)) {
                continue; // still loading: released once it is in
            }
            self.requested.1.retain(|q| q.look != r.look);
            let why = match slot {
                None => Some("no slot holds it any more"),
                Some((_, request, _)) if request != 0 => Some("the game asked for it"),
                Some(_) if game_wears(g, mgr, &r.param) => Some("one of the game's models wears it"),
                Some(_) => None,
            };
            if let Some(why) = why {
                self.events.push(format!("left the outfit {} to the game: {why}", hex(&r.look)));
            } else if let Some(param) = dress_as(g, g.scratch() + 0x30, &r.look, r.param[3]) {
                g.call(dll.code + RELEASE_DRESS, &[sys, param]);
                self.events.push(format!("released the outfit {}", hex(&r.look)));
            }
        }
    }

    fn delete(&mut self, g: &mut impl Calls, mgr: u32, pup: &Puppet, why: &str) {
        // the slot is one of this manager's 32 (a model's start, inside the array) and still holds this puppet
        let pool = g.u32(mgr.wrapping_add(field::MGR_POOL)).unwrap_or(0);
        let off = pup.mm.wrapping_sub(pool);
        let inside = pool != 0 && off < u32::from(field::POOL_SLOTS) * field::MM_SIZE && off % field::MM_SIZE == 0;
        if inside && owned(g, pup) {
            // the log says what state a dressed puppet's parts were in at its Delete
            let sys = g.u32(mgr.wrapping_add(MGR_MODEL_SYS)).unwrap_or(0);
            let parts = match self.requested.1.iter().find(|r| pup.worn == Some(r.look)) {
                _ if pup.worn.is_none() => String::new(),
                None => "; outfit not in xr-net's list".into(),
                Some(r) => match dress_slot(g, sys, &r.param) {
                    Some((i, request, state)) => format!("; its outfit in slot {i}, request {request} state {state}"),
                    None => "; its outfit in no slot".into(),
                },
            };
            g.call(DELETE, &[mgr, pup.mm]);
            self.events.push(format!("delete session {} (0x{:04X}): {why}{parts}", pup.session, pup.id));
        }
    }
}

/// `void Dress(u16 body[0x20], const u32 fashion[4])` for a boy (code.bin 0x12EA10) and a girl (0x12ED30): the game's
/// check (0x16A350 / 0x16A584; out of range: the default outfit), then skin, hair colour and one fashion item id per
/// category (a base table, code.bin 0x542004 for the boy, plus the field's value - 1; -1 for none) into a 0x40-byte
/// dress-up body.
pub const DRESS_BOY: u32 = 0x0012_EA10;
pub const DRESS_GIRL: u32 = 0x0012_ED30;
/// Guest bytes `dress` uses from its `at`: the 12-byte header, the 0x40-byte body at +0x10, the fashion at +0x50.
pub const DRESS_SIZE: u32 = 0x60;
/// Manager + this: u32 pointer to the field's model system (also every move model's +0x8B0), which keeps the dress-up
/// part sets: 20 slots of 16 bytes from +0x1DF8 (u8 request: 1 load, 4 unload; u8 state: 3 loaded; the 12-byte
/// parameter at +4, its body a heap copy).
pub const MGR_MODEL_SYS: u32 = 0x14B0;
pub const SYS_DRESS_SLOTS: u32 = 0x1DF8;
pub const DRESS_SLOTS: u32 = 20;
/// DllField code offsets (add DllField's code segment address): `RequestDressUp(sys, param)` finds the slot holding
/// that outfit or a free one, copies the parameter in and asks for its parts to load; `ReleaseDressUp(sys, param)`
/// asks for that slot to unload. A dressed model whose outfit has no loaded slot is not drawn (DllField+0x63820 finds
/// no slot and the draw handler retries every update), so it appears once the parts are in.
pub const REQUEST_DRESS: u32 = 0x52428;
pub const RELEASE_DRESS: u32 = 0x5250C;
/// A dress-up slot: u8 request, u8 state (RequestDressUp takes a slot only with both 0), the 12-byte parameter.
pub const DRESS_SLOT_SIZE: u32 = 0x10;
/// Free dress-up slots left to the game (the player's next outfit at a boutique, cutscene trainers).
pub const DRESS_KEEP: u32 = 2;
/// The model heap an outfit adds to a puppet (PUPPET_HEAP), in the worse heap. Laverre Pokemon Center: a
/// dressed puppet with PUPPET_MARK took 73,024 (0x14B) and 134,592 (0x14C) bytes in all, a plain one 54,224 and
/// 16,480 (with the player's byte 3 a dressed one had taken 34,896 and 6,048), so 160 KB.
pub const DRESS_HEAP: u32 = 160 * 1024;
/// Ticks dressed spawns wait for a busy outfit loader before they come plain (a load took up to about 280 frames
/// outdoors; a stuck slot must not keep every outfit away).
pub const DRESS_WAIT: u32 = 600;
/// A dress-up parameter: the 12-byte header and the 0x40-byte body.
pub const PARAM_SIZE: usize = 0x4C;
/// Header byte 3 of a boy puppet's parameter: 0, the value the game's NPC default path writes (the player's own is 1).
/// The game's compare (code.bin 0x4681C4: bytes 0-3, the two u16, then the body) then never matches a puppet's
/// outfit with the player's, so the two never share a dress-up slot: the game's ReleaseDressUp of the player's old
/// outfit cannot unload parts a puppet wears, and xr-net's cannot unload the player's. A girl's is PLAYER_MARK:
/// on the NPC set (0) her long hair is drawn as a plank and short hair not at all, as the wardrobe found;
/// the player's set takes one of the game's two player heaps, so `spawn` gates it (and never shares the game's slot):
/// a girl who can't have it wears her outfit on the NPC set, seen with the plank.
pub const PUPPET_MARK: u8 = 0;

/// A parameter's bytes for matching (the body pointer zeroed, the body after the header). None when unreadable.
pub(crate) fn param_bytes(g: &mut impl Guest, param: u32) -> Option<[u8; PARAM_SIZE]> {
    let mut b = [0u8; PARAM_SIZE];
    if !g.read(param, &mut b[..12]) {
        return None;
    }
    let body = u32::from_le_bytes(b[8..12].try_into().unwrap());
    b[8..12].fill(0);
    (body != 0 && g.read(body, &mut b[12..])).then_some(b)
}

/// The dress-up slot holding the outfit `param` (bytes as `param_bytes`): (index, request, state).
pub fn dress_slot(g: &mut impl Guest, sys: u32, param: &[u8; PARAM_SIZE]) -> Option<(u32, u8, u8)> {
    (0..DRESS_SLOTS).find_map(|i| {
        let at = sys.wrapping_add(SYS_DRESS_SLOTS + i * DRESS_SLOT_SIZE);
        let mut rs = [0u8; 2];
        (g.read(at, &mut rs) && (rs != [0, 0]) && param_bytes(g, at + 4).as_ref() == Some(param)).then_some((i, rs[0], rs[1]))
    })
}

/// The outfit's slot is unloading (request 4 / state 5) or gone: its parts are freed or about to be.
fn missing(g: &mut impl Guest, sys: u32, param: &[u8; PARAM_SIZE]) -> bool {
    !dress_slot(g, sys, param).is_some_and(|(_, request, state)| request != 4 && state != 5)
}

/// A model in the pool (the game's own or a puppet) wears the outfit `param` (its +0x998).
fn game_wears(g: &mut impl Guest, mgr: u32, param: &[u8; PARAM_SIZE]) -> bool {
    let slots = field::slots(g, mgr).unwrap_or_default();
    slots.iter().any(|s| param_bytes(g, s.addr + MM_DRESS).as_ref() == Some(param))
}

/// The player model for a gender (DllField+0x1C8EC: 2 for a boy, 1 for a girl).
pub fn player_model(gender: u8) -> u16 {
    if gender == 1 { MODEL_GIRL } else { MODEL_BOY }
}

/// Dress-up slots RequestDressUp could take (request and state 0). 0 when the model system cannot be read.
pub fn free_dress_slots(g: &mut impl Guest, sys: u32) -> u32 {
    (0..DRESS_SLOTS).filter(|i| g.u16(sys.wrapping_add(SYS_DRESS_SLOTS + i * DRESS_SLOT_SIZE)) == Some(0)).count() as u32
}

/// A dress-up slot is asked to load or unload, or is doing it (request 1 or 4; state 1, 2 or 5). Unreadable: busy.
pub fn dress_busy(g: &mut impl Guest, sys: u32) -> bool {
    (0..DRESS_SLOTS).any(|i| {
        let mut b = [0u8; 2];
        !g.read(sys.wrapping_add(SYS_DRESS_SLOTS + i * DRESS_SLOT_SIZE), &mut b) || b[0] != 0 || matches!(b[1], 1 | 2 | 5)
    })
}

/// An outfit as hex, for the log.
pub fn hex(a: &Appearance) -> String {
    a.fashion.iter().map(|b| format!("{b:02x}")).collect()
}

/// Builds the dress-up parameter for `a` at `at` (DRESS_SIZE bytes of guest memory) and returns its address: Create's
/// `extra` (its 4th argument). Create's init (code.bin 0x3D90F4) copies it into the model's +0x998 (0x46E1BC) when the
/// model can be dressed (model byte +0x88B, set for the player models 1 and 2), and the player model's draw handler
/// (DllField+0xDA72C) then builds it dressed. With `extra` 0 the same models get the default outfit. None for an
/// appearance that fails `validate` (a trust boundary: `Target.look` is a public field).
pub fn dress(g: &mut impl Calls, at: u32, a: &field::Appearance) -> Option<u32> {
    if !a.validate() {
        return None;
    }
    let (body, fashion) = (at + 0x10, at + 0x50);
    if !(g.write(body, &[0; 0x40]) && g.write(fashion, &a.fashion)) {
        return None;
    }
    g.call(if a.gender == 1 { DRESS_GIRL } else { DRESS_BOY }, &[body, fashion]);
    // the player's own header but for a boy's byte 3 (PUPPET_MARK; a girl's is PLAYER_MARK): byte 1 is the body's
    // layout (1 boy, 0 girl), then 0xFF, the mark, two u16 -1, the body
    let mark = if a.gender == 1 { PLAYER_MARK } else { PUPPET_MARK };
    let mut h = [0u8; 12];
    h[..8].copy_from_slice(&[0, u8::from(a.gender != 1), 0xFF, mark, 0xFF, 0xFF, 0xFF, 0xFF]);
    h[8..].copy_from_slice(&body.to_le_bytes());
    g.write(at, &h).then_some(at)
}

/// `dress` on the part set `mark` (header byte 3): an outfit asked for again or released goes as it was asked for.
fn dress_as(g: &mut impl Calls, at: u32, a: &field::Appearance, mark: u8) -> Option<u32> {
    let pa = dress(g, at, a)?;
    g.write(pa + 3, &[mark]).then_some(pa)
}

/// Manager: u32 count and 0x18-byte entries (u16 model first) of the model resources the map has loaded.
const MGR_RES_COUNT: u32 = 0x14BC;
const MGR_RES: u32 = 0x14C0;
const RES_SIZE: u32 = 0x18;

/// The model's resource is loaded: in the manager's table, or held by a model in use (code.bin 0x3D4E78's two
/// lookups before it would load or hang).
pub fn resident(g: &mut impl Guest, mgr: u32, model: u16) -> bool {
    let n = g.u32(mgr + MGR_RES_COUNT).unwrap_or(0).min(256);
    (0..n).any(|i| g.u16(mgr + MGR_RES + i * RES_SIZE) == Some(model))
        || field::slots(g, mgr).is_some_and(|s| s.iter().any(|s| s.model == model))
}

/// The slot still holds this puppet: in use, its id, and the marks only puppets carry together: an id from 0x8000
/// (X's zone data has 2,524 NPC entries in 360 zones, ids at most 0x1C), a player model (1 or 2: no zone entry uses
/// one; the player has id 0xFF) and script 2000. Not its zone: the zone field is not ours to rely on.
fn owned(g: &mut impl Guest, pup: &Puppet) -> bool {
    g.u32(pup.mm) == Some(field::MM_VTABLE)
        && g.u32(pup.mm + field::MM_STATUS).is_some_and(|s| s & 1 != 0)
        && g.u16(pup.mm + field::MM_ID) == Some(pup.id)
        && pup.id >= ID_BASE
        && g.u16(pup.mm + field::MM_MODEL).is_some_and(|m| m == MODEL_BOY || m == MODEL_GIRL)
        && g.u16(pup.mm + MM_SCRIPT) == Some(NO_SCRIPT)
}

/// Move model: u16 script id (zone entry +0x0A).
const MM_SCRIPT: u32 = 0x18;
/// Move model: the dress-up parameter (0x4C bytes; the save entry's +0x9C).
pub const MM_DRESS: u32 = 0x998;

/// A 0x30-byte zone NPC header (the zone data's layout; pk3DS OWSE) for a puppet standing at `t`: no movement code,
/// no event flag, no script, grid position from the world position, height as sent.
pub fn header(id: u16, model: u16, t: &Target) -> [u8; 0x30] {
    let mut h = [0u8; 0x30];
    let mut put = |off: usize, v: &[u8]| h[off..off + v.len()].copy_from_slice(v);
    put(0x00, &id.to_le_bytes());
    put(0x02, &model.to_le_bytes());
    put(0x0A, &NO_SCRIPT.to_le_bytes());
    put(0x0C, &u16::from(t.facing.min(3)).to_le_bytes());
    // as every zone entry has them: move area 1x1, no reuse key (0x1C == 0x1E: Create takes a free slot)
    put(0x18, &1u16.to_le_bytes());
    put(0x1A, &1u16.to_le_bytes());
    put(0x1C, &[0xFF; 6]);
    let tile = |v: f32| ((v / TILE).floor().clamp(0.0, 65535.0)) as u16;
    put(0x28, &tile(t.pos[0]).to_le_bytes());
    put(0x2A, &tile(t.pos[2]).to_le_bytes());
    put(0x2C, &t.pos[1].to_le_bytes());
    h
}

/// The model's own transform with the position replaced, through SetTransform.
fn set_pos(g: &mut impl Calls, mm: u32, pos: [f32; 3]) {
    let mut b = [0u8; TRANSFORM_SIZE];
    if !g.read(mm + TRANSFORM, &mut b) {
        return;
    }
    for (i, v) in pos.iter().enumerate() {
        b[4 + 4 * i..8 + 4 * i].copy_from_slice(&v.to_le_bytes());
    }
    let at = g.scratch();
    if g.write(at, &b) {
        g.call(SET_POS, &[mm, at]);
    }
}

/// World units a puppet tick at each mode's speed. The tick runs once a frame (60 Hz), but the field moves models at
/// 30 Hz: the controller's speeds per field update (walk 18/7, run 18/4, skates 4.35 to 6.48) are halved, so a walker
/// takes 14 frames a tile as the player does.
pub fn speed(mode: u8) -> f32 {
    match mode {
        redex_proto::MODE_RUN => TILE / 8.0,
        redex_proto::MODE_SKATES => 2.7,
        // the bike: 9 per field update (DllField controller +0x38 on the 100% save's bike)
        redex_proto::MODE_BIKE => TILE / 4.0,
        _ => TILE / 14.0,
    }
}

/// eidos's step: toward `to`, at most `by`.
fn towards(v: f32, to: f32, by: f32) -> f32 {
    if to - v > by {
        v + by
    } else if v - to > by {
        v - by
    } else {
        to
    }
}

/// A remote player's target as far as it is trusted (the server relays what another client sent): None when a
/// coordinate (or the local player's at `me`) is not a finite number; else x and z pulled in to MAX_REACH of the local
/// player and not below 0, and the height to MAX_RISE of theirs.
pub fn near(t: &Target, me: [f32; 3]) -> Option<Target> {
    if !t.pos.iter().chain(&me).all(|v| v.is_finite()) {
        return None;
    }
    let reach = |v: f32, me: f32, by: f32, floor: f32| v.clamp((me - by).max(floor), (me + by).max(floor));
    let pos = [reach(t.pos[0], me[0], MAX_REACH, 0.0), reach(t.pos[1], me[1], MAX_RISE, f32::MIN), reach(t.pos[2], me[2], MAX_REACH, 0.0)];
    Some(Target { pos, ..*t })
}

/// One tick of the walk toward `t` (eidos's step_avatar): the new position, and whether it moved. A target that is
/// not a finite number is not walked to.
pub fn walk(pos: [f32; 3], t: &Target) -> ([f32; 3], bool) {
    if !t.pos.iter().all(|v| v.is_finite()) {
        return (pos, false);
    }
    let lag = (t.pos[0] - pos[0]).abs().max((t.pos[2] - pos[2]).abs());
    if lag > SNAP {
        return (t.pos, pos != t.pos);
    }
    // eidos's sixth of the lag per (30 Hz) update: a twelfth a frame
    let mut by = speed(t.mode);
    if lag > CATCH_UP && lag / 12.0 > by {
        by = lag / 12.0;
    }
    let next = [towards(pos[0], t.pos[0], by), towards(pos[1], t.pos[1], by), towards(pos[2], t.pos[2], by)];
    (next, next != pos)
}

/// Status 2's NO_SAVE on (no setter in the game: a plain write). Checked every tick, in case anything rewrites status 2.
fn unsaved(g: &mut impl Calls, mm: u32) {
    if let Some(s2) = g.u32(mm + field::MM_STATUS2).filter(|s2| s2 & field::NO_SAVE == 0) {
        g.write(mm + field::MM_STATUS2, &(s2 | field::NO_SAVE).to_le_bytes());
    }
}

/// One tick of a puppet toward `t`: SetTransform puts it on the remote player's path, and it steps in place (walking
/// or running by their mode) while it moves. Not the walk and run motions: those are one-tile step actions that move
/// the model themselves from its grid tile at each field update (every other frame) and fight SetTransform (grid maps:
/// the drawn puppet jumped up to 4 tiles for a few frames; Lumiose City: it walked the other way).
fn step(g: &mut impl Calls, pup: &mut Puppet, t: &Target) {
    unsaved(g, pup.mm);
    // not set up yet (status bit 1, set by code.bin 0x3D7800 on the model's first update)
    if g.u32(pup.mm + field::MM_STATUS).is_none_or(|s| s & ACTIVE == 0) {
        return;
    }
    let (pos, moved) = walk(pup.pos, t);
    // anything else that moved the model is undone: it stays where xr-net puts it
    let drifted = [0, 4, 8].iter().zip(pos).any(|(&o, v)| g.f32(pup.mm + field::MM_POS + o).is_some_and(|w| w != v));
    if moved || drifted {
        set_pos(g, pup.mm, pos);
        pup.pos = pos;
    }
    if t.facing != pup.facing && t.facing <= 3 {
        g.call(SET_DIR, &[pup.mm, u32::from(t.facing)]);
        pup.facing = t.facing;
    }
    // positions come at 10 Hz: a walker can arrive a few frames before the next one, so it stands only after STAND_AFTER
    pup.still = if moved { 0 } else { pup.still.saturating_add(1) };
    let motion = match (pup.still < STAND_AFTER, t.mode) {
        (false, _) => MOTION_STAND,
        (true, redex_proto::MODE_WALK) => MOTION_WALK_IN_PLACE,
        // the bike and the skates too, at their own speed: the bicycle is not drawn under a puppet (the
        // riding motion 77 without it looks wrong; docs/re/x-field-move-models.md, "The bicycle on a puppet")
        (true, _) => MOTION_RUN_IN_PLACE,
    };
    // a step in place lasts one step: the next one starts when it ends, for as long as the puppet moves
    let ended = motion != MOTION_STAND && g.u32(pup.mm + MM_ACTION_STATE) != Some(1);
    if pup.motion != Some((motion, pup.facing)) || ended {
        g.call(SET_MOTION, &[pup.mm, motion, MOTION_DIR[usize::from(pup.facing.min(3))]]);
        pup.motion = Some((motion, pup.facing));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::wardrobe::PLAYER_HEAPS;
    use std::collections::HashMap;

    #[test]
    fn walks_like_eidos() {
        let t = |x: f32, mode| Target { pos: [x, 0.0, 0.0], facing: 3, mode, gender: 0, look: None, rank: 0 };
        // a walk frame, a run frame, arriving exactly
        assert_eq!(walk([0.0; 3], &t(18.0, 0)).0[0], TILE / 14.0);
        assert_eq!(walk([0.0; 3], &t(18.0, 1)).0[0], TILE / 8.0);
        assert_eq!(walk([17.0, 0.0, 0.0], &t(18.0, 0)), ([18.0, 0.0, 0.0], true));
        assert_eq!(walk([18.0, 0.0, 0.0], &t(18.0, 0)), ([18.0, 0.0, 0.0], false));
        // 1.5 tiles behind: a twelfth of the lag (2.25) beats the walk
        assert_eq!(walk([0.0; 3], &t(27.0, 0)).0[0], 2.25);
        // past two tiles: there at once
        assert_eq!(walk([0.0; 3], &t(37.0, 0)).0[0], 37.0);
        // never faster than the lag rule allows, so no rubber band beyond walking speed when on time
        let mut p = [0.0f32; 3];
        for _ in 0..14 {
            p = walk(p, &t(18.0, 0)).0;
        }
        assert!((p[0] - 18.0).abs() < 1e-3);
    }

    const TOBIN: Appearance = Appearance { gender: 0, fashion: [0x88, 0xB9, 0x19, 0x05, 0xD0, 0x09, 0x32, 0x03, 0x11, 0, 0, 0, 0, 0, 0, 0] };
    const RED: Appearance = Appearance { gender: 0, fashion: [0x48, 0x64, 0x09, 0x01, 0x01, 0xCB, 0x08, 0x03, 0x41, 0, 0, 0, 0, 0, 0, 0] };
    const PYJAMAS: Appearance = Appearance { gender: 0, fashion: [0x48, 0x04, 0x09, 0x01, 0x0A, 0x6A, 0x00, 0x00, 0x41, 0, 0, 0, 0, 0, 0, 0] };
    const GIRL3: Appearance = Appearance { gender: 1, fashion: [0xC0, 0, 0x24, 2, 0x4C, 5, 0x90, 2, 0, 4, 0, 0, 0, 0, 0, 0] };

    /// `dress` hands the fashion to the game's converter for the gender and points the header at the body, as the
    /// player's own (mm+0x998: 00 01 FF 01, FF FF FF FF, the body).
    #[test]
    fn dress_builds_the_players_header() {
        let mut g = Game::new(10, 200);
        let at = g.scratch() + 0x30;
        let girl = field::Appearance { gender: 1, fashion: [0xC0, 0, 0x24, 2, 0x4C, 5, 0x90, 2, 0, 4, 0, 0, 0, 0, 0, 0] };
        assert_eq!(dress(&mut g, at, &girl), Some(at));
        assert_eq!(g.calls.last(), Some(&(DRESS_GIRL, vec![at + 0x10, at + 0x50])));
        let mut h = [0u8; 12];
        g.read(at, &mut h);
        assert_eq!(h[..8], [0, 0, 0xFF, PLAYER_MARK, 0xFF, 0xFF, 0xFF, 0xFF], "a girl: the player's part set");
        assert_eq!(u32::from_le_bytes(h[8..].try_into().unwrap()), at + 0x10);
        let mut f = [0u8; 16];
        g.read(at + 0x50, &mut f);
        assert_eq!(f, girl.fashion);
        let boy = [0x88, 0xB9, 0x19, 0x05, 0xD0, 0x09, 0x32, 0x03, 0x11, 0, 0, 0, 0, 0, 0, 0];
        dress(&mut g, at, &field::Appearance { gender: 0, fashion: boy });
        assert_eq!(g.calls.last().unwrap().0, DRESS_BOY);
        g.read(at, &mut h);
        assert_eq!(h[1], 1, "the boy's body layout");
        assert_eq!(h[3], PUPPET_MARK, "a boy: the NPC set");
        // an appearance that fails the game's ranges never reaches the converter, in release builds too
        let calls = g.calls.len();
        let bad = field::Appearance { gender: 0, fashion: [0xFF; 16] };
        assert_eq!((dress(&mut g, at, &bad), g.calls.len()), (None, calls));
    }

    #[test]
    fn header_is_a_zone_entry() {
        let h = header(0x8001, MODEL_GIRL, &Target { pos: [8667.0, 144.0, 9135.0], facing: 1, mode: 0, gender: 1, look: None, rank: 0 });
        let u16_at = |o: usize| u16::from_le_bytes([h[o], h[o + 1]]);
        assert_eq!((u16_at(0), u16_at(2), u16_at(0x0A), u16_at(0x0C)), (0x8001, 1, NO_SCRIPT, 1));
        assert_eq!((u16_at(0x1C), u16_at(0x1E), u16_at(0x20)), (0xFFFF, 0xFFFF, 0xFFFF));
        // tile 481, 507 (x = tile * 18 + 9), height 144.0
        assert_eq!((u16_at(0x28), u16_at(0x2A)), (481, 507));
        assert_eq!(&h[0x2C..0x30], &144.0f32.to_le_bytes());
    }

    /// A fake game: guest memory as a byte map, and Create/Delete/SetPosition/SetDir/SetStatusBit done the way the
    /// game does them on a 32-slot pool.
    struct Game {
        mem: HashMap<u32, u8>,
        calls: Vec<(u32, Vec<u32>)>,
        data: u32,
        mgr: u32,
        pool: u32,
        /// the model system's dress-up slots: the outfit (header and body) each holds
        dress: [Option<Vec<u8>>; DRESS_SLOTS as usize],
    }

    /// DllField's code segment in the fake game.
    const CODE: u32 = 0x006A_5180;
    const SYS: u32 = 0x0900_0000;
    /// the dress-up loader's globals: +0x2C the local player's player heap, +0x30 the other (0: free)
    const HEAPS: u32 = 0x0A80_0000;

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
                CREATE => {
                    assert!(resident(self, args[0], u16::from_le_bytes([self.mem[&(args[1] + 2)], self.mem[&(args[1] + 3)]])), "Create of a model not loaded: the game hangs here");
                    // `extra`: the dress-up parameter goes to +0x998 (header, then the body, which the header points to)
                    let worn = (args[3] != 0).then(|| self.outfit(args[3]));
                    let hdr: Vec<u8> = (0..0x30).map(|i| self.mem[&(args[1] + i)]).collect();
                    let pool = self.pool;
                    let slot = (0..32).map(|i| pool + i * field::MM_SIZE).find(|&a| self.status(a) & 1 == 0).expect("the pool is full: the game hangs here");
                    self.put(slot + field::MM_STATUS, &(1 | ACTIVE).to_le_bytes());
                    self.put(slot + field::MM_STATUS2, &0x1800u32.to_le_bytes());
                    self.put(slot + field::MM_ID, &hdr[0..2]);
                    self.put(slot + field::MM_ZONE, &(args[2] as u16).to_le_bytes());
                    self.put(slot + field::MM_MODEL, &hdr[2..4]);
                    self.put(slot + MM_SCRIPT, &hdr[0x0A..0x0C]);
                    // the game points the copied header at the copied body, inside the model (+0x9A4)
                    let worn = worn.map(|mut w| {
                        w[8..12].copy_from_slice(&(slot + MM_DRESS + 12).to_le_bytes());
                        w
                    });
                    self.put(slot + MM_DRESS, &worn.unwrap_or(vec![0; 0x4C]));
                    slot
                }
                DRESS_BOY | DRESS_GIRL => {
                    // a body the fashion decides (the real one: item ids from it)
                    let f: Vec<u8> = (0..16).map(|i| self.mem[&(args[1] + i)]).collect();
                    self.put(args[0], &f);
                    0
                }
                f if f == CODE + REQUEST_DRESS => {
                    assert_eq!(args[0], SYS);
                    let want = self.outfit(args[1]);
                    // the game's: a slot that holds it (nothing to do once loaded), else a free one; none free: nothing
                    // (a loaded slot that holds it gets request 0, as the game's: an unload asked for is cancelled)
                    if let Some(i) = (0..DRESS_SLOTS).find(|&i| self.dress[i as usize].as_ref() == Some(&want)) {
                        self.put(SYS + SYS_DRESS_SLOTS + i * DRESS_SLOT_SIZE, &[0]);
                    } else {
                        if let Some(i) = (0..DRESS_SLOTS).find(|&i| self.dress[i as usize].is_none() && self.slot_state(i) == 0) {
                            // the player's part set takes the free player heap; with none free the real loader faults
                            if want[3] == PLAYER_MARK {
                                assert_eq!(self.u32(HEAPS + 0x30), Some(0), "both player heaps taken: the loader faults");
                                self.put(HEAPS + 0x30, &0x0B00_0000u32.to_le_bytes());
                            }
                            // loads at once in the fake
                            self.put_slot(i, Some(want));
                        }
                    }
                    0
                }
                f if f == CODE + RELEASE_DRESS => {
                    let want = self.outfit(args[1]);
                    for i in 0..DRESS_SLOTS {
                        if self.dress[i as usize].as_ref() == Some(&want) {
                            if want[3] == PLAYER_MARK && i != 0 {
                                self.put(HEAPS + 0x30, &[0; 4]);
                            }
                            self.put_slot(i, None);
                        }
                    }
                    0
                }
                DELETE => {
                    self.put(args[1] + field::MM_STATUS, &0u32.to_le_bytes());
                    self.put(args[1] + field::MM_STATUS2, &0x1800u32.to_le_bytes());
                    0
                }
                SET_STATUS => {
                    let s = self.status(args[0]) | args[1];
                    self.put(args[0] + field::MM_STATUS, &s.to_le_bytes());
                    0
                }
                SET_POS => {
                    let v: Vec<u8> = (4..16).map(|i| self.mem[&(args[1] + i)]).collect();
                    self.put(args[0] + field::MM_POS, &v);
                    0
                }
                _ => 0,
            }
        }
        fn scratch(&self) -> u32 {
            0x0FFF_FE00
        }
    }

    impl Game {
        fn new(npcs: u32, map: u16) -> Game {
            let (data, ctrl, mgr, pool): (u32, u32, u32, u32) = (0x0818_20F0, 0x082B_D8B4, 0x08D0_1F5C, 0x08D0_3924);
            let mut g = Game { mem: HashMap::new(), calls: Vec::new(), data, mgr, pool, dress: Default::default() };
            let snap = data + field::SNAPSHOT;
            g.put(snap + field::SNAP_CONTROLLER, &ctrl.to_le_bytes());
            g.put(ctrl + field::CTRL_MANAGER, &mgr.to_le_bytes());
            g.put(mgr + field::MGR_POOL, &pool.to_le_bytes());
            g.put(mgr + field::MGR_COUNT, &32u16.to_le_bytes());
            for i in 0..32 {
                g.put(pool + i * field::MM_SIZE, &field::MM_VTABLE.to_le_bytes());
            }
            // the player in slot npcs, NPCs below it
            for i in 0..=npcs {
                let a = pool + i * field::MM_SIZE;
                g.put(a + field::MM_STATUS, &1u32.to_le_bytes());
                g.put(a + field::MM_ID, &(if i == npcs { 0xFF } else { i as u16 }).to_le_bytes());
            }
            g.put(ctrl + field::CTRL_MOVE_MODEL, &(pool + npcs * field::MM_SIZE).to_le_bytes());
            // the player stands where the tests' players are
            for (i, v) in [8667.0f32, 144.0, 9135.0].iter().enumerate() {
                g.put(snap + field::SNAP_POS + 4 * i as u32, &v.to_le_bytes());
            }
            g.put(pool + npcs * field::MM_SIZE + field::MM_MODEL, &MODEL_BOY.to_le_bytes());
            // the map's loaded models: the player's and a girl's (tests that want her missing clear it)
            g.put(mgr + MGR_RES_COUNT, &2u32.to_le_bytes());
            g.put(mgr + MGR_RES, &MODEL_BOY.to_le_bytes());
            g.put(mgr + MGR_RES + RES_SIZE, &MODEL_GIRL.to_le_bytes());
            g.set_map(map);
            // the local player wears the save trainer's outfit (the 100% save's), loaded in dress-up slot 0
            g.put(mgr + MGR_MODEL_SYS, &SYS.to_le_bytes());
            g.put(PLAYER_HEAPS, &HEAPS.to_le_bytes());
            g.put(HEAPS + 0x2C, &0x0B80_0000u32.to_le_bytes());
            g.set_local(TOBIN);
            let local = g.local_outfit(TOBIN);
            g.put_slot(0, Some(local));
            g.calls.clear();
            g
        }
        /// The local player's own parameter for `a`: a puppet's with the player's header byte 3 (1).
        fn local_outfit(&mut self, a: Appearance) -> Vec<u8> {
            let at = self.scratch() + 0x30;
            dress(self, at, &a);
            let mut o = self.outfit(at);
            o[3] = 1;
            o
        }
        /// Dress-up slot `i` loaded with an outfit (header and body; the body copied to a heap of its own), or freed.
        fn put_slot(&mut self, i: u32, o: Option<Vec<u8>>) {
            let at = SYS + SYS_DRESS_SLOTS + i * DRESS_SLOT_SIZE;
            let body = 0x0A00_0000 + i * 0x40;
            match &o {
                Some(o) => {
                    self.put(at, &[0, 3]);
                    self.put(at + 4, &o[..8]);
                    self.put(at + 12, &body.to_le_bytes());
                    self.put(body, &o[12..]);
                }
                None => self.put(at, &[0; 16]),
            }
            self.dress[i as usize] = o;
        }
        fn dll(&self) -> Dll {
            Dll { code: CODE, data: self.data }
        }
        /// The local player's outfit, through GameData's MyStatus as the field reads it.
        fn set_local(&mut self, a: Appearance) {
            let (ctx, gd, ms) = (0x08C6_705Cu32, 0x08C6_727Cu32, 0x08C7_9C24u32);
            self.put(self.data + field::SNAPSHOT + field::SNAP_FIELDCTX, &ctx.to_le_bytes());
            self.put(ctx, &gd.to_le_bytes());
            self.put(gd, &field::GAMEDATA_VTABLE.to_le_bytes());
            self.put(gd + field::GD_MYSTATUS, &ms.to_le_bytes());
            self.put(ms, &field::MYSTATUS_VTABLE.to_le_bytes());
            self.put(ms + field::MS_DATA + field::MS_GENDER, &[a.gender]);
            self.put(ms + field::MS_DATA + field::MS_FASHION, &a.fashion);
        }
        /// A dress-up parameter's header and body.
        fn outfit(&mut self, param: u32) -> Vec<u8> {
            let mut b = vec![0u8; 0x4C];
            self.read(param, &mut b[..12]);
            let body = u32::from_le_bytes(b[8..12].try_into().unwrap());
            self.read(body, &mut b[12..]);
            b[8..12].fill(0);
            b
        }
        /// The slot holding the outfit a puppet wears (its +0x998).
        fn slot_of(&mut self, mm: u32) -> u32 {
            let p = param_bytes(self, mm + MM_DRESS).unwrap();
            dress_slot(self, SYS, &p).unwrap().0
        }
        fn slot_state(&mut self, i: u32) -> u16 {
            self.u16(SYS + SYS_DRESS_SLOTS + i * DRESS_SLOT_SIZE).unwrap()
        }
        fn first_worn(&mut self) -> Option<[u8; 16]> {
            let mm = self.puppets()[0];
            self.worn(mm)
        }
        /// Pool slots in use.
        fn used(&mut self) -> usize {
            let m = self.mgr;
            field::slots(self, m).unwrap().len()
        }
        /// Dress-up slots in use (request or state not 0).
        fn dress_used(&mut self) -> usize {
            (0..DRESS_SLOTS).filter(|&i| self.slot_state(i) != 0).count()
        }
        /// The outfit a puppet was made with: its +0x998 body's first 16 bytes (the fake converter's), or None.
        fn worn(&mut self, mm: u32) -> Option<[u8; 16]> {
            let mut b = [0u8; 16];
            self.read(mm + MM_DRESS + 12, &mut b);
            (self.u32(mm + MM_DRESS + 8) != Some(0)).then_some(b)
        }
        fn put(&mut self, addr: u32, b: &[u8]) {
            for (i, v) in b.iter().enumerate() {
                self.mem.insert(addr + i as u32, *v);
            }
        }
        fn status(&mut self, a: u32) -> u32 {
            self.u32(a + field::MM_STATUS).unwrap()
        }
        fn set_map(&mut self, map: u16) {
            self.put(self.data + field::SNAPSHOT + field::SNAP_MAP, &map.to_le_bytes());
            for i in 0..32 {
                let a = self.pool + i * field::MM_SIZE;
                if self.status(a) & 1 != 0 && self.u16(a + field::MM_ID) != Some(0xFF) && self.u16(a + field::MM_ID).unwrap() < ID_BASE {
                    self.put(a + field::MM_ZONE, &map.to_le_bytes());
                }
            }
        }
        /// The game's DeleteAll at a map change: every slot but the player's.
        fn delete_all(&mut self) {
            for i in 0..32 {
                let a = self.pool + i * field::MM_SIZE;
                if self.u16(a + field::MM_ID) != Some(0xFF) {
                    self.put(a + field::MM_STATUS, &0u32.to_le_bytes());
                }
            }
        }
        fn puppets(&mut self) -> Vec<u32> {
            let s = field::slots(self, self.mgr).unwrap();
            s.iter().filter(|s| s.id >= ID_BASE).map(|s| s.addr).collect()
        }
        fn count(&self, f: u32) -> usize {
            self.calls.iter().filter(|c| c.0 == f).count()
        }
    }

    fn target(x: f32, gender: u8) -> Target {
        Target { pos: [x, 144.0, 9135.0], facing: 1, mode: 0, gender, look: None, rank: 0 }
    }

    /// Ticks until a settled map shows all MAX_PUPPETS (one spawn per SPAWN_GAP).
    const FILL: u32 = SETTLE + 1 + SPAWN_GAP * MAX_PUPPETS as u32;

    #[test]
    fn hostile_positions_are_dropped_or_pulled_in() {
        let me = [8667.0, 144.0, 9135.0];
        let t = |pos| Target { pos, ..target(0.0, 0) };
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            for i in 0..3 {
                let mut pos = me;
                pos[i] = bad;
                assert_eq!(near(&t(pos), me), None, "{pos:?}");
                assert_eq!(walk(me, &t(pos)), (me, false), "never walked to: {pos:?}");
            }
        }
        assert_eq!(near(&t(me), [f32::NAN, 0.0, 0.0]), None, "the local player not a number: nothing is shown");
        // huge: pulled in to MAX_REACH (x, z) and MAX_RISE (y) of the local player
        let huge = near(&t([f32::MAX, -f32::MAX, 1e30]), me).unwrap().pos;
        assert_eq!(huge, [me[0] + MAX_REACH, me[1] - MAX_RISE, me[2] + MAX_REACH]);
        // negative x and z: never below 0; a local player off the map keeps the bounds ordered (clamp would panic)
        let low = near(&t([-50.0, 150.0, -1e9]), [100.0, 0.0, 0.0]).unwrap().pos;
        assert_eq!(low, [0.0, MAX_RISE, 0.0]);
        let edge = near(&t([-50.0, 0.0, -50.0]), [-1e6, 0.0, -1e6]).unwrap().pos;
        assert_eq!(edge, [0.0, 0.0, 0.0]);
        // in the game: a NaN target never becomes a puppet, a huge one stands at the reach
        let mut g = Game::new(10, 200);
        let mut p = loaded(200);
        p.want(200, vec![(1, t([f32::NAN, 144.0, 9135.0])), (2, t([1e38, 1e38, -1e38]))]);
        ticks(&mut p, &mut g, FILL);
        let shown = p.shown();
        assert_eq!(shown.len(), 1);
        assert_eq!((shown[0].0, shown[0].2), (2, [8667.0 + MAX_REACH, 144.0 + MAX_RISE, 9135.0 - MAX_REACH]));
        assert_eq!(g.f32(shown[0].1 + field::MM_POS), Some(8667.0 + MAX_REACH));
        // a shown player whose position turns into NaN is dropped like one who left; nothing NaN reaches the model
        let sets = g.count(SET_POS);
        p.want(200, vec![(2, t([f32::NAN, 144.0, 9135.0]))]);
        ticks(&mut p, &mut g, 3);
        assert!(p.shown().is_empty() && g.count(SET_POS) == sets);
    }

    #[test]
    fn churn_is_rate_limited() {
        let mut g = Game::new(10, 200);
        let mut p = loaded(200);
        // spawns come SPAWN_GAP ticks apart
        p.want(200, vec![(7, target(8667.0, 0)), (9, target(8703.0, 0))]);
        ticks(&mut p, &mut g, SETTLE + 1);
        assert_eq!(g.count(CREATE), 1);
        ticks(&mut p, &mut g, SPAWN_GAP - 1);
        assert_eq!(g.count(CREATE), 1);
        ticks(&mut p, &mut g, 1);
        assert_eq!(g.count(CREATE), 2);
        // a server that drops and announces session 7 every other tick: one Delete, then nothing for RESPAWN_GAP
        for k in 0..RESPAWN_GAP {
            let w = if k % 2 == 0 { vec![(9, target(8703.0, 0))] } else { vec![(7, target(8667.0, 0)), (9, target(8703.0, 0))] };
            p.want(200, w);
            ticks(&mut p, &mut g, 1);
        }
        assert_eq!((g.count(CREATE), g.count(DELETE)), (2, 1));
        p.want(200, vec![(7, target(8667.0, 0)), (9, target(8703.0, 0))]);
        ticks(&mut p, &mut g, 1);
        assert_eq!(g.count(CREATE), 3, "back after RESPAWN_GAP");
    }

    #[test]
    fn each_model_heap_must_have_room() {
        use super::super::heap::LINEAR_ROOT;
        // the root, and the two model heaps as its children, each with one free block of `a` and `b` bytes
        let heaps = |g: &mut Game, a: u32, b: u32| {
            let head = |g: &mut Game, at: u32, next: u32, child: u32, id: u32, free: u32| {
                g.put(at, &0x4558_5048u32.to_le_bytes());
                g.put(at + 8, &next.to_le_bytes());
                g.put(at + 0xC, &child.to_le_bytes());
                g.put(at + 0x24, &free.to_le_bytes());
                g.put(at + 0x50, &id.to_le_bytes());
            };
            head(g, LINEAR_ROOT, 0, 0x1400_1000, 1, 0);
            head(g, 0x1400_1000, 0x1400_2000, 0, 0x14B, 0x1400_3000);
            head(g, 0x1400_2000, 0, 0, 0x14C, 0x1400_4000);
            for (at, size) in [(0x1400_3000u32, a), (0x1400_4000, b)] {
                g.put(at, &0x4652u16.to_le_bytes());
                g.put(at + 4, &size.to_le_bytes());
                g.put(at + 0xC, &0u32.to_le_bytes());
            }
        };
        // 0x14C is nearly full: no puppet, though both heaps together have far more than one needs
        let mut g = Game::new(10, 200);
        heaps(&mut g, 600 * 1024, PUPPET_HEAP + HEAP_KEEP - 1);
        let mut p = loaded(200);
        p.want(200, vec![(7, target(8667.0, 0))]);
        ticks(&mut p, &mut g, FILL);
        assert!(g.puppets().is_empty());
        assert_eq!(p.hidden, 1);
        heaps(&mut g, 600 * 1024, PUPPET_HEAP + HEAP_KEEP);
        ticks(&mut p, &mut g, 1);
        assert_eq!(g.puppets().len(), 1);
    }

    /// Puppets on a map whose zone NPCs the (hooked) loader just loaded.
    fn loaded(map: u16) -> Puppets {
        let mut p = Puppets::new();
        p.zone_loaded(map);
        p
    }

    fn ticks(p: &mut Puppets, g: &mut Game, n: u32) {
        let d = Some(g.dll());
        for _ in 0..n {
            p.tick(g, d);
        }
    }

    #[test]
    fn spawn_move_despawn() {
        let mut g = Game::new(10, 200);
        let mut p = loaded(200);
        p.want(200, vec![(7, target(8667.0, 0)), (9, dressed(8703.0, GIRL3))]);
        ticks(&mut p, &mut g, SETTLE);
        assert!(g.puppets().is_empty(), "nothing before the map settles");
        ticks(&mut p, &mut g, 1 + SPAWN_GAP);
        let shown = g.puppets();
        assert_eq!(shown.len(), 2);
        for &a in &shown {
            assert_eq!(g.u32(a + field::MM_STATUS).unwrap() & NO_HIT, NO_HIT);
            assert_eq!(g.u32(a + field::MM_STATUS2).unwrap() & field::NO_SAVE, field::NO_SAVE);
        }
        assert_eq!(g.u16(shown[1] + field::MM_MODEL), Some(MODEL_GIRL));
        // the walk: one tile right takes 14 frames, as the player's (and one more for the float that is left)
        p.want(200, vec![(7, target(8685.0, 0)), (9, dressed(8703.0, GIRL3))]);
        ticks(&mut p, &mut g, 13);
        assert!(g.f32(shown[0] + field::MM_POS).unwrap() < 8685.0);
        ticks(&mut p, &mut g, 2);
        assert_eq!(g.f32(shown[0] + field::MM_POS), Some(8685.0));
        // arrived, the game's walk motion carries the model on: it is put back on the next tick
        g.put(shown[0] + field::MM_POS, &8709.0f32.to_le_bytes());
        ticks(&mut p, &mut g, 1);
        assert_eq!(g.f32(shown[0] + field::MM_POS), Some(8685.0));
        let sets = g.count(SET_POS);
        ticks(&mut p, &mut g, 3);
        assert_eq!(g.count(SET_POS), sets, "no move and no drift: nothing to put back");
        // session 9 leaves (disconnect, timeout or another map): deleted through the game
        p.want(200, vec![(7, target(8685.0, 0))]);
        ticks(&mut p, &mut g, 1);
        assert_eq!(g.puppets(), vec![shown[0]]);
        assert_eq!(g.count(DELETE), 1);
        // the remote player changes map: their puppet goes
        p.want(201, vec![(7, target(8685.0, 0))]);
        ticks(&mut p, &mut g, 1);
        assert!(g.puppets().is_empty());
    }

    #[test]
    fn map_change_forgets_what_the_game_deleted() {
        let mut g = Game::new(10, 200);
        let mut p = loaded(200);
        p.want(200, vec![(7, target(8667.0, 0))]);
        ticks(&mut p, &mut g, FILL);
        assert_eq!(g.puppets().len(), 1);
        // the local player walks through a door: the game empties the pool and loads the new zone's NPCs
        g.delete_all();
        g.set_map(202);
        p.zone_loaded(202);
        ticks(&mut p, &mut g, 1);
        assert!(p.shown().is_empty());
        assert_eq!(g.count(DELETE), 0, "the game already deleted it: never delete a slot that is not ours");
        // the remote player follows: back after the new map settles
        p.want(202, vec![(7, target(369.0, 0))]);
        ticks(&mut p, &mut g, SETTLE);
        assert_eq!(g.puppets().len(), 1);
    }

    #[test]
    fn a_full_pool_shows_fewer_and_never_hangs() {
        let mut g = Game::new(20, 200); // 21 used, 11 free: the pool limits
        let mut p = loaded(200);
        p.want(200, (0..40).map(|s| (s, target(8667.0 + s as f32, 0))).collect());
        ticks(&mut p, &mut g, FILL);
        assert_eq!(g.puppets().len(), 32 - 21 - MIN_FREE);
        assert_eq!(p.hidden, 40 - (32 - 21 - MIN_FREE));
        // the game's own NPCs are untouched
        assert_eq!(field::slots(&mut g, 0x08D0_1F5C).unwrap().iter().filter(|s| s.id < ID_BASE).count(), 21);
        // 9 used, 23 free: the cap limits
        let mut g = Game::new(8, 200);
        let mut p = loaded(200);
        p.want(200, (0..40).map(|s| (s, target(8667.0 + s as f32, 0))).collect());
        ticks(&mut p, &mut g, FILL);
        assert_eq!(g.puppets().len(), MAX_PUPPETS);
        assert_eq!(p.hidden, 40 - MAX_PUPPETS);
    }

    #[test]
    fn the_save_sweep_deletes_all_and_they_come_back() {
        let mut g = Game::new(10, 200);
        let mut p = loaded(200);
        p.want(200, vec![(7, target(8667.0, 0)), (9, target(8703.0, 0))]);
        ticks(&mut p, &mut g, FILL);
        assert_eq!(g.puppets().len(), 2);
        let dll = g.dll();
        p.sweep(&mut g, dll);
        assert!(g.puppets().is_empty(), "nothing of a puppet reaches the save");
        assert_eq!(g.count(DELETE), 2);
        ticks(&mut p, &mut g, FILL);
        assert_eq!(g.puppets().len(), 2);
    }

    #[test]
    fn a_model_not_loaded_falls_back_to_the_players() {
        let mut g = Game::new(10, 200);
        g.put(g.mgr + MGR_RES_COUNT, &1u32.to_le_bytes());
        let mut p = loaded(200);
        p.want(200, vec![(9, target(8703.0, 1))]);
        ticks(&mut p, &mut g, FILL);
        let shown = g.puppets();
        assert_eq!(shown.len(), 1);
        assert_eq!(g.u16(shown[0] + field::MM_MODEL), Some(MODEL_BOY));
    }

    #[test]
    fn motion_counts_steps_jumps_and_reversals() {
        let mut m = Motion::default();
        let mut pup = Puppet { session: 1, mm: 0, id: ID_BASE, zone: 0, pos: [0.0; 3], facing: 0, motion: None, still: 0, last_step: [0.0; 2], look: None, worn: None, model: 0 };
        for x in [2.0, 4.0, 3.0, 3.0, 50.0] {
            let before = pup.pos;
            pup.pos[0] = x;
            m.record(&mut pup, before);
        }
        // 4 -> 3 turns back at walking speed (not counted), 3 -> 50 turns back with a jump
        assert_eq!(m, Motion { steps: 4, max_step: 47.0, jumps: 1, reversals: 1 });
    }

    #[test]
    fn puppets_step_in_place_where_they_are_put() {
        let mut g = Game::new(10, 200);
        let mut p = loaded(200);
        p.want(200, vec![(7, target(8667.0, 0))]);
        ticks(&mut p, &mut g, FILL);
        let mm = g.puppets()[0];
        // the remote player walks right, a tile ahead; before some frames something moves the model (a walk action)
        for k in 0..40 {
            if k % 2 == 0 {
                let x = g.f32(mm + field::MM_POS).unwrap();
                g.put(mm + field::MM_POS, &(x + 7.0).to_le_bytes());
            }
            p.want(200, vec![(7, Target { facing: 3, ..target(8685.0 + k as f32 * TILE / 14.0, 0) })]);
            ticks(&mut p, &mut g, 1);
            assert_eq!(g.f32(mm + field::MM_POS), p.shown().first().map(|s| s.2[0]), "frame {k}: not where xr-net put it");
        }
        // it moved, stepping in place (never the step actions that move the model), facing right
        assert!(g.f32(mm + field::MM_POS).unwrap() > 8667.0 + TILE);
        let motions: Vec<&Vec<u32>> = g.calls.iter().filter(|c| c.0 == SET_MOTION).map(|c| &c.1).collect();
        assert!(motions.iter().all(|m| m[1] != MOTION_WALK && m[1] != MOTION_RUN));
        assert_eq!(motions.last().map(|m| (m[1], m[2])), Some((MOTION_WALK_IN_PLACE, 3)));
        // the fake game never ends an action (state 0): a step in place is set again every frame; once ended in the
        // game it is set again too, and while it plays (state 1) it is left alone
        g.put(mm + MM_ACTION_STATE, &1u32.to_le_bytes());
        let n = g.count(SET_MOTION);
        p.want(200, vec![(7, Target { facing: 3, ..target(8740.0, 0) })]);
        ticks(&mut p, &mut g, 2);
        assert_eq!(g.count(SET_MOTION), n, "a step in place that still plays is not set again");
        // the remote player stops: after STAND_AFTER frames the puppet stands, facing them (down: SetMotion's 0)
        p.want(200, vec![(7, Target { facing: 1, ..target(g.f32(mm + field::MM_POS).unwrap(), 0) })]);
        ticks(&mut p, &mut g, STAND_AFTER + 1);
        let last = g.calls.iter().rev().find(|c| c.0 == SET_MOTION).map(|c| (c.1[1], c.1[2]));
        assert_eq!(last, Some((MOTION_STAND, 0)));
    }

    #[test]
    fn the_fallback_is_always_a_player_model() {
        // the local player in another form's model (neither 1 nor 2): a girl not loaded becomes a boy, never model 7
        let mut g = Game::new(10, 200);
        let player = g.pool + 10 * field::MM_SIZE;
        g.put(player + field::MM_MODEL, &7u16.to_le_bytes());
        g.put(g.mgr + MGR_RES, &7u16.to_le_bytes());
        g.put(g.mgr + MGR_RES + RES_SIZE, &MODEL_BOY.to_le_bytes());
        let mut p = loaded(200);
        p.want(200, vec![(9, target(8703.0, 1))]);
        ticks(&mut p, &mut g, FILL);
        let shown = g.puppets();
        assert_eq!(shown.len(), 1);
        assert_eq!(g.u16(shown[0] + field::MM_MODEL), Some(MODEL_BOY));
        // neither player model loaded: no puppet at all (Create would hang), and nothing is left to leak
        let mut g = Game::new(10, 200);
        g.put(player + field::MM_MODEL, &7u16.to_le_bytes());
        g.put(g.mgr + MGR_RES_COUNT, &1u32.to_le_bytes());
        g.put(g.mgr + MGR_RES, &7u16.to_le_bytes());
        let mut p = loaded(200);
        p.want(200, vec![(9, target(8703.0, 1))]);
        ticks(&mut p, &mut g, FILL);
        assert!(g.puppets().is_empty() && g.count(CREATE) == 0);
    }

    #[test]
    fn the_game_creating_into_a_full_pool_gets_a_puppets_slot() {
        let mut g = Game::new(10, 200);
        let mut p = loaded(200);
        p.want(200, (0..40).map(|s| (s, target(8667.0 + s as f32, 0))).collect());
        ticks(&mut p, &mut g, FILL);
        let shown = g.puppets().len();
        // the game fills the pool itself (a script adds NPCs), then wants one more
        for i in 0..(32 - 11 - shown) {
            let pool = g.pool;
            let a = (0..32).map(|k| pool + k * field::MM_SIZE).find(|&a| g.status(a) & 1 == 0).unwrap();
            g.put(a + field::MM_STATUS, &1u32.to_le_bytes());
            g.put(a + field::MM_ID, &(100 + i as u16).to_le_bytes());
        }
        let (dll, mgr) = (g.dll(), g.mgr);
        p.make_room(&mut g, dll, mgr);
        assert_eq!(g.puppets().len(), shown - 1);
        assert_eq!(field::slots(&mut g, mgr).unwrap().len(), 31);
        // a pool with room: nothing goes
        p.make_room(&mut g, dll, mgr);
        assert_eq!(g.puppets().len(), shown - 1);
    }

    #[test]
    fn the_nearest_are_shown_without_flicker() {
        // the player stands at x 8667 (the fake game's snapshot position is 0: put it there)
        let mut g = Game::new(8, 200);
        let snap = g.data + field::SNAPSHOT;
        g.put(snap + field::SNAP_POS, &8667.0f32.to_le_bytes());
        g.put(snap + field::SNAP_POS + 8, &9135.0f32.to_le_bytes());
        let mut p = loaded(200);
        // 12 players a tile apart going right: sessions 0..8 are the nearest
        let row = |far: f32| (0..12u32).map(|s| (s, target(8667.0 + TILE * (s as f32 + 1.0) + if s == 11 { far } else { 0.0 }, 0))).collect::<Vec<_>>();
        p.want(200, row(0.0));
        ticks(&mut p, &mut g, FILL);
        let shown = |p: &Puppets| {
            let mut v: Vec<u32> = p.shown().iter().map(|s| s.0).collect();
            v.sort();
            v
        };
        assert_eq!(shown(&p), (0..8).collect::<Vec<_>>());
        // session 11 comes to 2 tiles from the player: 6 tiles nearer than session 7, more than KEEP_MARGIN, so it
        // takes the farthest shown one's place
        p.want(200, row(-10.0 * TILE));
        ticks(&mut p, &mut g, 1);
        assert!(shown(&p).contains(&11) && !shown(&p).contains(&7), "{:?}", shown(&p));
        // session 7 then stands 1 tile nearer than session 6 is: inside the margin, nothing swaps
        let deletes = g.count(DELETE);
        let mut w = row(-10.0 * TILE);
        w[7].1.pos[0] = 8667.0 + TILE * 6.0;
        p.want(200, w.clone());
        ticks(&mut p, &mut g, 5);
        assert_eq!(g.count(DELETE), deletes, "no flicker within KEEP_MARGIN");
        // a friend (rank 1) far away is shown before the nearest strangers
        // swaps wait SWAP_GAP ticks after the last one
        ticks(&mut p, &mut g, SWAP_GAP);
        w[10].1.rank = 1;
        p.want(200, w);
        ticks(&mut p, &mut g, 1);
        assert!(shown(&p).contains(&10), "{:?}", shown(&p));
        assert_eq!(p.shown().len(), MAX_PUPPETS);
    }

    #[test]
    fn they_wait_for_the_zone_loader() {
        let mut g = Game::new(10, 200);
        let mut p = Puppets::new();
        p.want(200, vec![(7, target(8667.0, 0))]);
        ticks(&mut p, &mut g, SETTLE + 5);
        assert!(g.puppets().is_empty(), "no loader seen: not yet");
        p.zone_loaded(200);
        ticks(&mut p, &mut g, AFTER_LOAD - 1);
        assert!(g.puppets().is_empty());
        ticks(&mut p, &mut g, 2);
        assert_eq!(g.puppets().len(), 1);
        // a map whose loader is not hooked: after SETTLE_NO_LOADER
        let mut g = Game::new(10, 200);
        let mut p = loaded(999);
        p.want(200, vec![(7, target(8667.0, 0))]);
        ticks(&mut p, &mut g, SETTLE_NO_LOADER);
        assert!(g.puppets().is_empty());
        ticks(&mut p, &mut g, 1);
        assert_eq!(g.puppets().len(), 1);
    }

    #[test]
    fn a_lost_field_keeps_them_and_orphans_are_reaped() {
        let mut g = Game::new(10, 200);
        let mut p = loaded(200);
        p.want(200, (0..8).map(|s| (s, target(8667.0 + s as f32, 0))).collect());
        ticks(&mut p, &mut g, FILL);
        assert_eq!(g.puppets().len(), 8);
        // a battle: DllField goes (no data segment), the pool keeps the puppets; back, they are still known
        let calls = g.calls.len();
        p.tick(&mut g, None);
        assert_eq!((p.shown().len(), g.calls.len()), (8, calls), "nothing touched without the field");
        ticks(&mut p, &mut g, FILL);
        assert_eq!((g.puppets().len(), g.count(DELETE)), (8, 0));
        // puppets nobody knows (the state was lost): deleted before any new one, never more than MAX_PUPPETS
        let mut q = loaded(200);
        q.want(200, (0..8).map(|s| (s, target(8667.0 + s as f32, 0))).collect());
        ticks(&mut q, &mut g, 1);
        assert!(g.puppets().is_empty());
        assert_eq!(g.count(DELETE), 8);
        ticks(&mut q, &mut g, FILL);
        assert_eq!(g.puppets().len(), 8);
        // a sweep without the field touches nothing; with it, all go
        let dll = g.dll();
        g.put(dll.data + field::SNAPSHOT + field::SNAP_CONTROLLER, &0u32.to_le_bytes());
        q.sweep(&mut g, dll);
        assert_eq!(g.puppets().len(), 8);
        g.put(dll.data + field::SNAPSHOT + field::SNAP_CONTROLLER, &0x082B_D8B4u32.to_le_bytes());
        q.sweep(&mut g, dll);
        assert!(g.puppets().is_empty());
    }

    #[test]
    fn ids_stay_in_the_puppet_range() {
        let mut p = Puppets::new();
        p.next_id = 0x7FFE;
        let mut g = Game::new(10, 200);
        p.zone_loaded(200);
        p.want(200, vec![(1, target(8667.0, 0))]);
        ticks(&mut p, &mut g, FILL);
        let a = g.puppets()[0];
        assert_eq!(g.u16(a + field::MM_ID), Some(0xFFFE));
        assert_eq!(p.next_id, 0);
    }

    #[test]
    fn a_far_position_is_pulled_in() {
        let mut g = Game::new(10, 200);
        let snap = g.data + field::SNAPSHOT;
        g.put(snap + field::SNAP_POS, &8667.0f32.to_le_bytes());
        let mut p = loaded(200);
        p.want(200, vec![(1, target(8667.0 + 1000.0 * TILE, 0))]);
        ticks(&mut p, &mut g, FILL);
        assert_eq!(p.shown()[0].2[0], 8667.0 + MAX_REACH);
    }

    #[test]
    fn a_stale_load_does_not_count_on_a_return() {
        // map A loaded through the hook, then B and back to A without a hooked load: A's old run must not count
        let mut g = Game::new(10, 200);
        let mut p = loaded(200);
        p.want(200, vec![(7, target(8667.0, 0))]);
        ticks(&mut p, &mut g, FILL);
        assert_eq!(g.puppets().len(), 1);
        g.delete_all();
        g.set_map(201);
        p.want(201, vec![(7, target(8667.0, 0))]);
        ticks(&mut p, &mut g, LOAD_LEAD + 10);
        g.delete_all();
        g.set_map(200);
        p.want(200, vec![(7, target(8667.0, 0))]);
        ticks(&mut p, &mut g, SETTLE_NO_LOADER);
        assert!(g.puppets().is_empty(), "spawned on A's stale load");
        ticks(&mut p, &mut g, 1);
        assert_eq!(g.puppets().len(), 1, "the no-loader fallback");
    }

    #[test]
    fn only_marked_slots_are_reaped() {
        let mut g = Game::new(10, 200);
        // a game model with a high id but no puppet marks (a model other than the player's)
        let a = g.pool + 20 * field::MM_SIZE;
        g.put(a + field::MM_STATUS, &1u32.to_le_bytes());
        g.put(a + field::MM_ID, &0x9000u16.to_le_bytes());
        g.put(a + field::MM_MODEL, &50u16.to_le_bytes());
        let mut p = loaded(200);
        ticks(&mut p, &mut g, 2);
        assert_eq!(g.count(DELETE), 0, "not ours: never deleted");
        // with the marks (a puppet left by a lost state) it goes
        g.put(a + field::MM_MODEL, &MODEL_BOY.to_le_bytes());
        g.put(a + MM_SCRIPT, &NO_SCRIPT.to_le_bytes());
        ticks(&mut p, &mut g, 1);
        assert_eq!(g.count(DELETE), 1);
    }

    #[test]
    fn room_is_made_without_a_snapshot() {
        let mut g = Game::new(10, 200);
        let mut p = loaded(200);
        p.want(200, (0..8).map(|s| (s, target(8667.0 + s as f32, 0))).collect());
        ticks(&mut p, &mut g, FILL);
        let pool = g.pool;
        for k in 0..32 {
            let a = pool + k * field::MM_SIZE;
            if g.status(a) & 1 == 0 {
                g.put(a + field::MM_STATUS, &1u32.to_le_bytes());
                g.put(a + field::MM_ID, &(100 + k as u16).to_le_bytes());
            }
        }
        // a scripted event: the snapshot is all zeros, and the game wants a slot in the full pool
        let (dll, mgr) = (g.dll(), g.mgr);
        g.put(dll.data + field::SNAPSHOT + field::SNAP_POS, &[0; 12]);
        p.make_room(&mut g, dll, mgr);
        assert_eq!(field::slots(&mut g, mgr).unwrap().len(), 31);
        // another manager's Create: not ours to make room in
        p.make_room(&mut g, dll, 0x0900_0000);
        assert_eq!(g.count(DELETE), 1);
    }

    fn dressed(x: f32, look: Appearance) -> Target {
        Target { look: Some(look), ..target(x, look.gender) }
    }

    /// A remote player's outfit goes to Create (`extra`, so the model's +0x998 holds it) after RequestDressUp
    /// asked for its parts, and is released through the game when nobody wears it; the local player's own outfit is
    /// never asked for nor released (its slot is the game's).
    #[test]
    fn outfits_are_asked_for_worn_and_released() {
        let mut g = Game::new(10, 200);
        let mut p = loaded(200);
        p.want(200, vec![(7, dressed(8667.0, RED)), (8, dressed(8703.0, TOBIN))]);
        ticks(&mut p, &mut g, FILL);
        let shown = g.puppets();
        assert_eq!(shown.len(), 2);
        assert_eq!((g.worn(shown[0]), g.worn(shown[1])), (Some(RED.fashion), Some(TOBIN.fashion)));
        // both outfits loaded: both drawn; one slot back to loading: that puppet is not drawn (no name over nothing)
        assert_eq!(p.drawn(&mut g).len(), 2);
        let i = g.slot_of(shown[0]);
        g.put(SYS + SYS_DRESS_SLOTS + i * DRESS_SLOT_SIZE, &[0, 2]);
        assert_eq!(p.drawn(&mut g).iter().map(|d| d.0).collect::<Vec<_>>(), vec![8]);
        g.put(SYS + SYS_DRESS_SLOTS + i * DRESS_SLOT_SIZE, &[0, 3]);
        // the header as the player's own (boy layout) but for PUPPET_MARK, its body pointer moved into the model
        assert_eq!(g.u32(shown[0] + MM_DRESS), Some(0x00_FF_01_00));
        // the save trainer's twin of the local player gets a slot of its own: the player's (header byte 3 = 1) is never shared
        assert_eq!(g.count(CODE + REQUEST_DRESS), 2);
        assert_eq!(g.dress_used(), 3);
        assert_ne!(g.slot_of(shown[1]), 0);
        // both leave: their parts go RELEASE_AFTER ticks later (never in the tick of the Delete), the local player's stay
        p.want(200, vec![]);
        ticks(&mut p, &mut g, RELEASE_AFTER - 1);
        assert!(g.puppets().is_empty());
        assert_eq!(g.count(CODE + RELEASE_DRESS), 0);
        ticks(&mut p, &mut g, 1);
        assert_eq!((g.count(CODE + RELEASE_DRESS), g.dress_used()), (2, 1));
        assert!(g.dress[0].is_some(), "the local player's outfit is still loaded");
        // a plain look (None) is Create's extra 0
        p.want(200, vec![(9, target(8667.0, 0))]);
        ticks(&mut p, &mut g, RESPAWN_GAP + FILL);
        let mm = g.puppets()[0];
        assert_eq!((g.u16(mm + field::MM_MODEL), g.worn(mm)), (Some(MODEL_BOY), None));
        assert_eq!(g.calls.iter().rev().find(|c| c.0 == CREATE).unwrap().1[3], 0);
    }

    const GIRL4: Appearance = Appearance { gender: 1, fashion: [0x40, 0x3C, 0x24, 2, 0, 0x20, 0x13, 3, 0x41, 5, 0x20, 0, 0, 0, 0, 0] };

    fn mark(g: &mut Game, mm: u32) -> Option<u8> {
        g.u8(mm + MM_DRESS + 3)
    }

    /// A girl's outfit loads the player's part set (PLAYER_MARK; on the NPC set her long hair is a plank and
    /// short hair is not drawn), which takes one of the game's two player heaps. The local player's outfit holds one,
    /// so one girl outfit of ours at a time on the player's set: another girl in another outfit wears hers on the NPC
    /// set (seen, not a ghost), one in the same outfit shares its slot. A boy's keeps PUPPET_MARK.
    /// Once every girl in it leaves, the heap is free again for the next girl.
    #[test]
    fn a_girls_outfit_takes_the_free_player_heap_only() {
        let mut g = Game::new(10, 200);
        let mut p = loaded(200);
        p.want(200, vec![(7, dressed(8667.0, GIRL3)), (8, dressed(8703.0, GIRL4)), (9, dressed(8739.0, GIRL3)), (10, dressed(8775.0, RED))]);
        ticks(&mut p, &mut g, FILL);
        let shown = g.puppets();
        assert_eq!(shown.len(), 4);
        assert_eq!((g.worn(shown[0]), mark(&mut g, shown[0])), (Some(GIRL3.fashion), Some(PLAYER_MARK)), "the first girl: the player's set");
        assert_eq!((g.worn(shown[1]), mark(&mut g, shown[1])), (Some(GIRL4.fashion), Some(PUPPET_MARK)), "another girl outfit: no player heap left, the NPC set");
        assert_eq!((g.worn(shown[2]), mark(&mut g, shown[2])), (Some(GIRL3.fashion), Some(PLAYER_MARK)), "the same outfit shares its slot");
        assert_eq!(g.slot_of(shown[0]), g.slot_of(shown[2]));
        assert_ne!(g.slot_of(shown[0]), g.slot_of(shown[1]));
        assert_eq!((g.worn(shown[3]), mark(&mut g, shown[3])), (Some(RED.fashion), Some(PUPPET_MARK)), "a boy keeps the NPC set");
        assert_eq!(p.drawn(&mut g).len(), 4, "all three girls are drawn");
        // every girl leaves: the NPC-set outfit is released without touching the player heap, GIRL3's frees it
        p.want(200, vec![(10, dressed(8775.0, RED))]);
        ticks(&mut p, &mut g, RELEASE_AFTER + 1);
        assert_eq!(g.u32(HEAPS + 0x30), Some(0), "the player heap is free once her outfit is released");
        assert!(p.requested.1.iter().all(|r| r.look == RED));
        // the next girl to come gets it, on the player's set
        p.want(200, vec![(11, dressed(8667.0, GIRL4)), (10, dressed(8775.0, RED))]);
        ticks(&mut p, &mut g, RESPAWN_GAP + FILL);
        let girls: Vec<u32> = g.puppets().into_iter().filter(|&mm| g.u16(mm + field::MM_MODEL) == Some(MODEL_GIRL)).collect();
        assert_eq!(girls.len(), 1);
        assert_eq!((g.worn(girls[0]), mark(&mut g, girls[0])), (Some(GIRL4.fashion), Some(PLAYER_MARK)), "the heap is free again: the player's set");
        // both player heaps taken (the game's own change of clothes): a girl wears hers on the NPC set, no heap used
        let mut g = Game::new(10, 200);
        g.put(HEAPS + 0x30, &1u32.to_le_bytes());
        let mut p = loaded(200);
        p.want(200, vec![(7, dressed(8667.0, GIRL3))]);
        ticks(&mut p, &mut g, FILL);
        let mm = g.puppets()[0];
        assert_eq!((g.worn(mm), mark(&mut g, mm), g.u32(HEAPS + 0x30)), (Some(GIRL3.fashion), Some(PUPPET_MARK), Some(1)));
    }

    /// `want_also` adds to the frame's players on the same map (a session again is replaced), and starts over on another.
    #[test]
    fn want_also_adds_beside_the_world() {
        let mut p = Puppets::new();
        p.want(200, vec![(1, target(1.0, 0))]);
        p.want_also(200, vec![(0x1000, target(2.0, 1))]);
        p.want_also(200, vec![(0x1000, target(3.0, 1))]);
        let w = p.wanted.as_ref().unwrap();
        assert_eq!(w.1.iter().map(|(s, t)| (*s, t.pos[0])).collect::<Vec<_>>(), [(1, 1.0), (0x1000, 3.0)]);
        p.want_also(201, vec![(0x1000, target(4.0, 1))]);
        assert_eq!(p.wanted.as_ref().map(|w| (w.0, w.1.len())), Some((201, 1)));
    }

    /// The local player's twin (a girl puppet in the local player's exact outfit, whose slot the game holds) wears
    /// hers on the NPC set, in a slot of her own; never the game's.
    #[test]
    fn a_girl_twin_wears_her_outfit_on_the_npc_set() {
        let mut g = Game::new(10, 200);
        g.set_local(GIRL3);
        let own = g.local_outfit(GIRL3);
        g.put_slot(0, Some(own));
        let mut p = loaded(200);
        p.want(200, vec![(7, dressed(8667.0, GIRL3))]);
        ticks(&mut p, &mut g, FILL);
        let mm = g.puppets()[0];
        assert_eq!((g.worn(mm), mark(&mut g, mm)), (Some(GIRL3.fashion), Some(PUPPET_MARK)));
        assert_ne!(g.slot_of(mm), 0, "a slot of her own");
        assert_eq!(g.u32(HEAPS + 0x30), Some(0), "no player heap taken");
        // she leaves: hers is released, the game's stays
        p.want(200, vec![]);
        ticks(&mut p, &mut g, RELEASE_AFTER + 1);
        assert!(g.dress[0].is_some() && g.dress_used() == 1);
    }

    /// A shared girl outfit whose slot a field close emptied, while another outfit took the free player heap: a
    /// new girl in it waits (no RequestDressUp with both heaps taken, which the fake panics on as the loader faults), the
    /// worn one is not asked for again either; once the heap is free it is asked for again and she comes, sharing it.
    #[test]
    fn a_girl_waits_while_her_outfits_parts_are_gone() {
        let mut g = Game::new(10, 200);
        let mut p = loaded(200);
        p.want(200, vec![(7, dressed(8667.0, GIRL3))]);
        ticks(&mut p, &mut g, FILL);
        let first = g.puppets()[0];
        let slot = g.slot_of(first);
        // a battle's ReleaseAll unloaded it (its heap freed), and something else took that heap meanwhile
        g.put_slot(slot, None);
        g.put(HEAPS + 0x30, &1u32.to_le_bytes());
        let requests = g.count(CODE + REQUEST_DRESS);
        p.want(200, vec![(7, dressed(8667.0, GIRL3)), (8, dressed(8703.0, GIRL3))]);
        ticks(&mut p, &mut g, FILL);
        assert_eq!((g.puppets(), g.count(CODE + REQUEST_DRESS)), (vec![first], requests), "she waits; nothing asked for");
        // the heap is free again: asked for again (on the player's set), and the second girl shares it
        g.put(HEAPS + 0x30, &[0; 4]);
        ticks(&mut p, &mut g, FILL);
        let shown = g.puppets();
        assert_eq!(shown.len(), 2);
        assert_eq!(g.slot_of(shown[0]), g.slot_of(shown[1]));
        assert_eq!(mark(&mut g, shown[1]), Some(PLAYER_MARK));
    }

    /// A girl model that would wear nothing is not made (only her shadow, held item and name would show): a plain
    /// girl, and a boy on a girl stand-in model. Logged once.
    #[test]
    fn a_girl_model_without_an_outfit_is_not_made() {
        let mut g = Game::new(10, 200);
        let mut p = loaded(200);
        p.want(200, vec![(9, target(8703.0, 1))]);
        ticks(&mut p, &mut g, FILL);
        assert!(g.puppets().is_empty() && g.count(CREATE) == 0);
        assert_eq!(p.events.iter().filter(|e| e.contains("a girl model with no outfit")).count(), 1, "{:?}", p.events);
        // the local player in another form's model, only the girl model loaded: a boy would stand in on it, plain
        let mut g = Game::new(10, 200);
        let player = g.pool + 10 * field::MM_SIZE;
        g.put(player + field::MM_MODEL, &7u16.to_le_bytes());
        g.put(g.mgr + MGR_RES, &7u16.to_le_bytes());
        let mut p = loaded(200);
        p.want(200, vec![(9, dressed(8703.0, RED))]);
        ticks(&mut p, &mut g, FILL);
        assert!(g.puppets().is_empty() && g.count(CREATE) == 0);
    }

    /// A girl's outfit on a map where only the boy model is loaded: the stand-in boy wears the plain look (a girl's body
    /// on the boy model would be wrong), and nothing is asked for.
    #[test]
    fn a_stand_in_model_wears_the_plain_look() {
        let mut g = Game::new(10, 200);
        g.put(g.mgr + MGR_RES_COUNT, &1u32.to_le_bytes());
        let mut p = loaded(200);
        p.want(200, vec![(9, dressed(8703.0, GIRL3))]);
        ticks(&mut p, &mut g, FILL);
        let mm = g.puppets()[0];
        assert_eq!((g.u16(mm + field::MM_MODEL), g.worn(mm)), (Some(MODEL_BOY), None));
        assert_eq!(g.count(CODE + REQUEST_DRESS), 0);
    }

    /// The dress-up gates: with DRESS_KEEP or fewer free slots the puppet comes plain; while a slot loads, an outfit
    /// waits (one load at a time, so the heap gate sees each); without the local player's outfit it waits too.
    #[test]
    fn dress_slots_are_gated() {
        let mut g = Game::new(10, 200);
        for i in 1..DRESS_SLOTS - DRESS_KEEP {
            g.put(SYS + SYS_DRESS_SLOTS + i * DRESS_SLOT_SIZE, &[0, 3]);
        }
        let mut p = loaded(200);
        p.want(200, vec![(7, dressed(8667.0, RED))]);
        ticks(&mut p, &mut g, FILL);
        let mm = g.puppets()[0];
        assert_eq!(g.worn(mm), None, "no free slot beyond DRESS_KEEP: plain");
        assert_eq!(g.count(CODE + REQUEST_DRESS), 0);
        // a slot that loads holds the next outfit back; it comes once the load is done
        let mut g = Game::new(10, 200);
        g.put(SYS + SYS_DRESS_SLOTS + 5 * DRESS_SLOT_SIZE, &[1, 0]);
        let mut p = loaded(200);
        p.want(200, vec![(7, dressed(8667.0, RED)), (8, target(8703.0, 0))]);
        ticks(&mut p, &mut g, FILL);
        assert_eq!(g.puppets().len(), 1, "the plain one only");
        assert_eq!(p.hidden, 0, "waiting is not hidden");
        g.put(SYS + SYS_DRESS_SLOTS + 5 * DRESS_SLOT_SIZE, &[0, 3]);
        ticks(&mut p, &mut g, SPAWN_GAP + 1);
        assert_eq!(g.puppets().len(), 2);
        // a slot stuck loading: the dressed spawn waits (and says so), then comes plain after DRESS_WAIT
        let mut g = Game::new(10, 200);
        g.put(SYS + SYS_DRESS_SLOTS + 5 * DRESS_SLOT_SIZE, &[0, 2]);
        let mut p = loaded(200);
        p.want(200, vec![(7, dressed(8667.0, RED))]);
        ticks(&mut p, &mut g, DRESS_WAIT - 1);
        assert!(g.puppets().is_empty());
        assert!(p.events.iter().any(|e| e.contains("session 7 waits for the outfit loader")), "{:?}", p.events);
        ticks(&mut p, &mut g, 2);
        assert_eq!(g.first_worn(), None, "plain after DRESS_WAIT");
        assert!(p.events.iter().any(|e| e.contains("busy for 600 ticks")));
    }

    /// Each mode spawns and plays its motion in place: walking 26, running 27; the bike and the skates run in place
    /// at their own speed (no bicycle under a puppet yet: the fallback). A surfing player is not shown, and comes
    /// back when they leave the water.
    #[test]
    fn each_mode_shows_with_its_motion_and_surfers_hide() {
        for (mode, motion) in [(redex_proto::MODE_WALK, MOTION_WALK_IN_PLACE), (redex_proto::MODE_RUN, MOTION_RUN_IN_PLACE), (redex_proto::MODE_BIKE, MOTION_RUN_IN_PLACE), (redex_proto::MODE_SKATES, MOTION_RUN_IN_PLACE)] {
            let mut g = Game::new(10, 200);
            let mut p = loaded(200);
            p.want(200, vec![(7, Target { mode, ..dressed(8667.0, RED) })]);
            ticks(&mut p, &mut g, FILL);
            let mm = g.puppets()[0];
            assert_eq!(g.worn(mm), Some(RED.fashion));
            p.want(200, vec![(7, Target { mode, facing: 3, ..dressed(8667.0 + TILE, RED) })]);
            ticks(&mut p, &mut g, 2);
            let last = g.calls.iter().rev().find(|c| c.0 == SET_MOTION).map(|c| c.1[1]);
            assert_eq!(last, Some(motion), "mode {mode}");
            assert!((p.shown()[0].2[0] - (8667.0 + 2.0 * speed(mode))).abs() < 1e-2, "mode {mode} moves at its speed");
        }
        let mut g = Game::new(10, 200);
        let mut p = loaded(200);
        let surf = |x| Target { mode: redex_proto::MODE_SURF, ..dressed(x, RED) };
        p.want(200, vec![(7, surf(8667.0))]);
        ticks(&mut p, &mut g, FILL);
        assert!(g.puppets().is_empty() && g.count(CREATE) == 0 && p.hidden == 0, "a surfer is never made");
        // on land: shown; back in the water: deleted, its outfit released; on land again: back
        p.want(200, vec![(7, dressed(8667.0, RED))]);
        ticks(&mut p, &mut g, SPAWN_GAP + 1);
        assert_eq!(g.puppets().len(), 1);
        p.want(200, vec![(7, surf(8667.0))]);
        ticks(&mut p, &mut g, 1);
        assert!(g.puppets().is_empty());
        ticks(&mut p, &mut g, RELEASE_AFTER);
        assert_eq!((g.count(CODE + RELEASE_DRESS), g.dress_used()), (1, 1));
        p.want(200, vec![(7, dressed(8667.0, RED))]);
        ticks(&mut p, &mut g, RESPAWN_GAP + SPAWN_GAP);
        assert_eq!(g.puppets().len(), 1);
    }

    /// 50 outfit and mode changes mid-map: each is a Delete and a Create under the spawn gap, and afterwards the pool,
    /// the dress-up slots and the asked-for outfits are as they were (nothing leaks); the save sweep with a dressed
    /// biker in the pool leaves the slots as they were too.
    #[test]
    fn fifty_swaps_leave_nothing_behind() {
        let mut g = Game::new(10, 200);
        let mut p = loaded(200);
        let look = |k: u32| Target { mode: [redex_proto::MODE_BIKE, redex_proto::MODE_WALK, redex_proto::MODE_SKATES][k as usize % 3], ..dressed(8667.0, [RED, PYJAMAS, GIRL3, TOBIN][k as usize % 4]) };
        p.want(200, vec![(7, look(0))]);
        ticks(&mut p, &mut g, FILL);
        let (slots, dress) = (g.used(), g.dress_used());
        assert_eq!((slots, dress), (12, 2));
        for k in 1..=50 {
            p.want(200, vec![(7, look(k))]);
            // the old one goes at once, the new one after RESPAWN_GAP (a look that changes every frame costs a Delete
            // and a Create a second at most)
            ticks(&mut p, &mut g, 1);
            assert!(g.puppets().is_empty(), "swap {k}");
            ticks(&mut p, &mut g, RESPAWN_GAP + 1);
            let mm = g.puppets()[0];
            assert_eq!(g.worn(mm), Some(look(k).look.unwrap().fashion), "swap {k}");
            assert!(g.dress_used() <= 3, "swap {k}: {} dress-up slots", g.dress_used());
        }
        assert_eq!((g.count(CREATE), g.count(DELETE)), (51, 50));
        // back to the first look: the pool and the dress-up slots as before the swaps
        p.want(200, vec![(7, look(0))]);
        ticks(&mut p, &mut g, RESPAWN_GAP + 2);
        assert_eq!((g.used(), g.dress_used()), (slots, dress));
        // the save sweep: the dressed biker goes, its outfit after it; it comes back dressed
        let dll = g.dll();
        p.sweep(&mut g, dll);
        ticks(&mut p, &mut g, RELEASE_AFTER);
        assert_eq!((g.puppets().len(), g.dress_used()), (0, 1));
        // (the map's load is long past: the no-loader settle)
        ticks(&mut p, &mut g, SETTLE_NO_LOADER + 2);
        assert_eq!(g.first_worn(), Some(RED.fashion));
        // everyone leaves: only the local player's outfit is left
        p.want(200, vec![]);
        ticks(&mut p, &mut g, RELEASE_AFTER + 1);
        assert_eq!((g.used(), g.dress_used()), (11, 1));
        assert!(p.requested.1.is_empty());
    }

    /// The local player's twin (a puppet in their exact outfit) has a slot of its own: when the local player changes
    /// clothes the game releases its own slot, and the twin stays. A worn slot that starts unloading or goes anyway
    /// makes the puppet ask for the parts again (no Delete).
    #[test]
    fn the_local_players_twin_survives_their_change_of_clothes() {
        let mut g = Game::new(10, 200);
        let mut p = loaded(200);
        p.want(200, vec![(7, dressed(8667.0, TOBIN))]);
        ticks(&mut p, &mut g, FILL);
        let twin = g.puppets()[0];
        let slot = g.slot_of(twin);
        assert_ne!(slot, 0);
        // the local player changes clothes: the game asks for Red and releases the save trainer (its own parameter)
        g.set_local(RED);
        let (red, jorge) = (g.local_outfit(RED), g.local_outfit(TOBIN));
        g.put_slot(1 + DRESS_SLOTS - 2, Some(red));
        g.put_slot(0, None);
        assert!(jorge[3] == 1 && g.dress.iter().all(|d| d.as_ref() != Some(&jorge)));
        ticks(&mut p, &mut g, 2 * RELEASE_AFTER);
        assert_eq!((g.puppets(), g.count(DELETE)), (vec![twin], 0), "the twin is untouched");
        // its own slot starts unloading (the game's ReleaseAll at a field close): asked for again, never deleted
        let requests = g.count(CODE + REQUEST_DRESS);
        g.put(SYS + SYS_DRESS_SLOTS + slot * DRESS_SLOT_SIZE, &[4, 3]);
        ticks(&mut p, &mut g, 1);
        assert_eq!((g.puppets(), g.count(DELETE), g.count(CODE + REQUEST_DRESS)), (vec![twin], 0, requests + 1));
        assert!(p.events.iter().any(|e| e.contains("asked again for the outfit 88b9") && e.contains("no Delete")));
        // gone altogether (the unload finished): asked for again into a free slot, still no Delete
        g.put_slot(slot, None);
        ticks(&mut p, &mut g, 1);
        assert_eq!((g.puppets(), g.count(DELETE)), (vec![twin], 0));
        assert_eq!(g.first_worn(), Some(TOBIN.fashion));
        let mm = g.puppets()[0];
        g.slot_of(mm); // a slot holds it again
        ticks(&mut p, &mut g, 3);
        assert_eq!(g.count(CODE + REQUEST_DRESS), requests + 2, "asked for again only while it was missing");
        // the player leaves while the slot is gone: no Delete until a slot holds the outfit again
        let s = g.slot_of(mm);
        g.put_slot(s, None);
        let full = (0..DRESS_SLOTS).filter(|&i| g.slot_state(i) == 0).collect::<Vec<_>>();
        for &i in &full {
            g.put(SYS + SYS_DRESS_SLOTS + i * DRESS_SLOT_SIZE, &[0, 3]); // no free slot: asking again does nothing
        }
        p.want(200, vec![]);
        ticks(&mut p, &mut g, 5);
        assert_eq!((g.puppets(), g.count(DELETE)), (vec![twin], 0), "kept while its parts are gone");
        for &i in &full {
            g.put(SYS + SYS_DRESS_SLOTS + i * DRESS_SLOT_SIZE, &[0, 0]);
        }
        // asked for again into a free slot, then deleted with its parts loaded
        ticks(&mut p, &mut g, 2);
        assert!(g.puppets().is_empty());
        assert!(p.events.iter().any(|e| e.contains("): gone; its outfit in slot")), "{:?}", p.events);
    }

    /// The reverse race: inside RELEASE_AFTER the game asks for an outfit xr-net asked for (a request xr-net did not
    /// make), or one of its own models comes to wear it. xr-net then leaves the slot to the game: no release.
    #[test]
    fn a_slot_the_game_picks_up_is_never_released() {
        for game_wears in [false, true] {
            let mut g = Game::new(10, 200);
            let mut p = loaded(200);
            p.want(200, vec![(7, dressed(8667.0, RED))]);
            ticks(&mut p, &mut g, FILL);
            let mm = g.puppets()[0];
            let slot = g.slot_of(mm);
            if game_wears {
                // the player's model (pool slot 10) wears the same parameter
                let dress: Vec<u8> = (0..PARAM_SIZE as u32).map(|i| g.mem[&(mm + MM_DRESS + i)]).collect();
                let player = g.pool + 10 * field::MM_SIZE;
                g.put(player + MM_DRESS, &dress);
            }
            p.want(200, vec![]);
            ticks(&mut p, &mut g, RELEASE_AFTER / 2);
            if !game_wears {
                g.put(SYS + SYS_DRESS_SLOTS + slot * DRESS_SLOT_SIZE, &[1, 3]);
            }
            ticks(&mut p, &mut g, RELEASE_AFTER);
            assert_eq!(g.count(CODE + RELEASE_DRESS), 0, "game_wears {game_wears}");
            assert!(p.requested.1.is_empty());
            assert!(p.events.iter().any(|e| e.starts_with("left the outfit 4864") && e.contains("to the game")), "{:?}", p.events);
        }
    }

    /// A slot still loading when its outfit has been unworn for RELEASE_AFTER waits for the load before the release.
    #[test]
    fn a_loading_slot_is_released_once_loaded() {
        let mut g = Game::new(10, 200);
        let mut p = loaded(200);
        p.want(200, vec![(7, dressed(8667.0, RED))]);
        ticks(&mut p, &mut g, FILL);
        let mm = g.puppets()[0];
        let slot = g.slot_of(mm);
        g.put(SYS + SYS_DRESS_SLOTS + slot * DRESS_SLOT_SIZE, &[0, 2]);
        p.want(200, vec![]);
        ticks(&mut p, &mut g, RELEASE_AFTER + 5);
        assert_eq!(g.count(CODE + RELEASE_DRESS), 0);
        g.put(SYS + SYS_DRESS_SLOTS + slot * DRESS_SLOT_SIZE, &[0, 3]);
        ticks(&mut p, &mut g, 1);
        assert_eq!((g.count(CODE + RELEASE_DRESS), g.dress_used()), (1, 1));
    }

    #[test]
    fn no_field_no_touching() {
        let mut g = Game::new(10, 200);
        let mut p = loaded(200);
        p.want(200, vec![(7, target(8667.0, 0))]);
        ticks(&mut p, &mut g, FILL);
        let before = g.calls.len();
        p.tick(&mut g, None);
        assert_eq!(p.shown().len(), 1);
        assert_eq!(g.calls.len(), before);
    }

    /// When the game needs a slot and no puppet's parts are safe to delete first, the newest
    /// goes as before; but a newest puppet whose outfit's slot is unloading is passed over for one whose parts are in.
    #[test]
    fn make_room_deletes_a_puppet_with_loaded_parts_first() {
        let mut g = Game::new(10, 200);
        let mut p = loaded(200);
        p.want(200, vec![(1, target(8667.0, 0))]);
        ticks(&mut p, &mut g, FILL);
        p.want(200, vec![(1, target(8667.0, 0)), (2, dressed(8685.0, RED))]);
        ticks(&mut p, &mut g, FILL);
        let (plain, dressed_mm) = (g.puppets()[0], g.puppets()[1]);
        assert_eq!(p.live.last().map(|x| x.mm), Some(dressed_mm), "the dressed one is the newest");
        // its slot starts unloading (a field close): its parts are going
        let slot = g.slot_of(dressed_mm);
        g.put(SYS + SYS_DRESS_SLOTS + slot * DRESS_SLOT_SIZE, &[4, 3]);
        let shown = g.puppets().len();
        for i in 0..(32 - 11 - shown) {
            let pool = g.pool;
            let a = (0..32).map(|k| pool + k * field::MM_SIZE).find(|&a| g.status(a) & 1 == 0).unwrap();
            g.put(a + field::MM_STATUS, &1u32.to_le_bytes());
            g.put(a + field::MM_ID, &(100 + i as u16).to_le_bytes());
        }
        let (dll, mgr) = (g.dll(), g.mgr);
        p.make_room(&mut g, dll, mgr);
        assert_eq!(g.puppets(), vec![dressed_mm], "the plain puppet went, the one with going parts stayed (plain was {plain:#x})");
    }

    /// A session id reused after a world switch by a player of the other gender is made again (the model differs).
    #[test]
    fn a_reused_session_of_the_other_gender_is_made_again() {
        let mut g = Game::new(10, 200);
        let mut p = loaded(200);
        p.want(200, vec![(9, dressed(8703.0, GIRL3))]);
        ticks(&mut p, &mut g, FILL);
        assert_eq!(g.puppets().iter().map(|&m| g.u16(m + field::MM_MODEL)).collect::<Vec<_>>(), vec![Some(MODEL_GIRL)]);
        p.want(200, vec![(9, target(8703.0, 0))]);
        ticks(&mut p, &mut g, 2 * FILL);
        assert_eq!(g.count(DELETE), 1);
        assert_eq!(g.puppets().iter().map(|&m| g.u16(m + field::MM_MODEL)).collect::<Vec<_>>(), vec![Some(MODEL_BOY)]);
        assert!(p.events.iter().any(|e| e.contains("outfit change")), "{:?}", p.events);
    }

    /// A puppet wearing the parameter counts too: another puppet's outfit slot is never released under it.
    #[test]
    fn a_puppet_wearing_the_parameter_keeps_the_slot() {
        let mut g = Game::new(10, 200);
        let mut p = loaded(200);
        p.want(200, vec![(7, dressed(8667.0, RED))]);
        ticks(&mut p, &mut g, FILL);
        let mm = g.puppets()[0];
        let mgr = g.mgr;
        let param = param_bytes(&mut g, mm + MM_DRESS).unwrap();
        assert!(game_wears(&mut g, mgr, &param), "a model in the pool wears it, puppet or not");
    }
}
