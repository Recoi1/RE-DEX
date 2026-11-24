//! `--character`: the character's look goes into the game's MyStatus between frames until the field shows it (a new
//! game: gender, name and outfit; Continue: the outfit, for a save of the character's gender), and the look changes the
//! game makes after that come back to be kept in `characters.json`.
use crate::{exec, field_data, log, Mem};
use xr_net::x::field::{self, Appearance, MS_FASHION, MS_GENDER, MS_NAME, MS_NICKNAME, MS_SPRITE};
use std::sync::atomic::{AtomicU64, Ordering};
use xr_net::x::look::Look;

/// The app heap MiB GameData sits in: XR_MEMDUMP of the 100% save in the field, `[ctx+0]` = 0x08C6727C (the same in X
/// and Y, new game and Continue, scripted and free-running).
const GAMEDATA_REGION: std::ops::Range<u32> = 0x08C0_0000..0x08D0_0000;
/// The name's UTF-16 units in MyStatus (12 and a terminator).
const NAME_UNITS: usize = 13;
/// MyStatus's data block up to the nickname's end: what `step` needs.
pub const MS_LEN: usize = MS_NICKNAME as usize + 2 * NAME_UNITS;
/// A new game looks for its first save this often (frames).
const SAVE_POLL: u64 = 60;
/// MyStatus in the `main` save file.
const SAVE_MYSTATUS: u64 = 0x14000;

pub struct LookSync {
    look: Appearance,
    name: [u16; NAME_UNITS],
    /// the field's look once it showed: the last valid one of the character's gender
    seen: Option<Appearance>,
    /// a save the player made is on disk (Continue: from the start). Until then the writes are a new game's (gender,
    /// name and outfit), and the intro's outfits (the pyjamas, Mom's mirror) are not the character's
    saved: bool,
    /// this load's writes reached MyStatus
    wrote: bool,
    /// the first save was just made: the character's skin, hair and eyes go back over the intro's (Mom's mirror sets
    /// the hair style), once, in the clothes the game has on
    restore: bool,
    /// the gender whose starting clothes the game must fill in (`FILL_CLOTHES`): the intro picked the other one
    clothes: Option<u8>,
    /// the save file's (length, mtime) at the last poll: a save counts once it stops changing
    stamp: Option<(u64, Option<std::time::SystemTime>)>,
    /// DllTitle was loaded at the last frame
    title: bool,
    gamedata: Option<u32>,
    frames: u64,
}

impl LookSync {
    pub fn new(look: Appearance, name: &str, new_game: bool) -> LookSync {
        let mut units = [0; NAME_UNITS];
        for (u, c) in units.iter_mut().zip(name.encode_utf16().take(NAME_UNITS - 1)) {
            *u = c;
        }
        LookSync { look, name: units, seen: None, saved: !new_game, wrote: false, restore: false, clothes: None, stamp: None, title: false, gamedata: None, frames: 0 }
    }

    /// `ms`: MyStatus's data block (at least 0x62 bytes) once GameData is found; `field`: the look the field reads now.
    /// The look the game changed to, once per change, after the field first showed.
    pub fn step(&mut self, ms: Option<&mut [u8]>, field: Option<Appearance>) -> Option<Appearance> {
        let Some(last) = self.seen else {
            match field {
                None => self.write(ms?),
                Some(f) => {
                    if let Some(line) = self.verdict(f) {
                        log::log(&line);
                    }
                    self.seen = field;
                }
            }
            return None;
        };
        // nothing is taken while the first save's restore waits for MyStatus: it looks at what changed itself
        if self.restore {
            return self.restore_features(ms?, last);
        }
        // a look of the other gender (a save of the other gender) never becomes the character's
        let f = field.filter(|f| *f != last && f.gender == self.look.gender && f.validate())?;
        self.seen = Some(f);
        if !self.saved {
            log::log(&format!("character: the intro changed the look to {} (not kept: no save yet)", hex(&f.to_bytes())));
            return None;
        }
        self.look = f;
        log::log(&format!("character: the game changed the look to {}", hex(&f.to_bytes())));
        Some(f)
    }

    /// The first save's one write: each of the character's skin, hair style, hair colour and eyes that MyStatus still
    /// has as the intro left it (`intro`, the look seen at the save; Mom's mirror sets the hair style) goes back, on the
    /// clothes MyStatus has now. A change the player made since the save (a boutique, a salon) is theirs and kept: then
    /// the result is reported. The game draws it at the next map change.
    fn restore_features(&mut self, ms: &mut [u8], intro: Appearance) -> Option<Appearance> {
        self.restore = false;
        let (g, at) = (MS_GENDER as usize, MS_FASHION as usize);
        let now = Appearance { gender: ms[g], fashion: ms[at..at + 16].try_into().unwrap() };
        if now.gender != self.look.gender || !now.validate() {
            return None;
        }
        let (n, i, c) = (Look::of(&now), Look::of(&intro), Look::of(&self.look));
        let pick = |n: u8, i: u8, c: u8| if n == i { c } else { n };
        let features = Look { skin: pick(n.skin, i.skin, c.skin), hair_style: pick(n.hair_style, i.hair_style, c.hair_style), hair_colour: pick(n.hair_colour, i.hair_colour, c.hair_colour), eyes: pick(n.eyes, i.eyes, c.eyes), ..c };
        let back = features.wearing(now.fashion)?;
        ms[at..at + 16].copy_from_slice(&back.fashion);
        self.seen = Some(back);
        log::log(&format!("character: the first save: the character's skin, hair and eyes back in MyStatus, {}", hex(&back.to_bytes())));
        if now == intro {
            return None;
        }
        self.look = back;
        log::log(&format!("character: the game changed the look to {} (since the first save)", hex(&back.to_bytes())));
        Some(back)
    }

    /// The log line for the field's first look after the writes: applied only when it is the character's and was written.
    fn verdict(&self, f: Appearance) -> Option<String> {
        let what = if self.saved { "continue" } else { "new game" };
        if f.gender != self.look.gender {
            Some(format!("character: the save is a {}, the character a {}: look not applied", gender(f.gender), gender(self.look.gender)))
        } else if f != self.look {
            Some(format!("character: look not applied: the field shows {} ({what})", hex(&f.to_bytes())))
        } else {
            self.wrote.then(|| format!("character: look applied {} ({what})", hex(&f.to_bytes())))
        }
    }

    /// Before the field: before the first save what the intro sets with the gender and the name (gender, sprite bytes,
    /// name, nickname) and the outfit; after it the outfit (a save of the character's gender). Over an intro of the
    /// other gender, the first write also asks for that gender's starting clothes (`clothes`).
    fn write(&mut self, ms: &mut [u8]) {
        let (g, at) = (MS_GENDER as usize, MS_FASHION as usize);
        if !self.saved {
            if !self.wrote && ms[g] != self.look.gender {
                self.clothes = Some(self.look.gender);
            }
            ms[g] = self.look.gender;
            ms[MS_SPRITE as usize..][..2].fill(if self.look.gender == 0 { 1 } else { 0 });
            for (i, u) in self.name.iter().enumerate() {
                for name in [MS_NAME, MS_NICKNAME] {
                    ms[name as usize + 2 * i..][..2].copy_from_slice(&u.to_le_bytes());
                }
            }
        }
        if ms[g] == self.look.gender {
            ms[at..at + 16].copy_from_slice(&self.look.fashion);
            self.wrote = true;
        }
    }

    /// The new game's first save is on disk: the game's look changes count from now on, after the character's skin,
    /// hair and eyes go back in.
    pub fn saved(&mut self) {
        (self.saved, self.restore) = (true, true);
    }

    /// The game started again (a soft reset: the title is up): its next field gets the character's look again.
    pub fn rearm(&mut self) {
        if self.seen.is_some() {
            log::log("character: the game started again: the look goes in again before the field");
        }
        (self.seen, self.wrote) = (None, false);
    }

    /// After a frame: GameData found once, then `step` on MyStatus and the field's look.
    pub fn frame(&mut self, m: &mut exec::Machine) -> Option<Appearance> {
        self.frames += 1;
        if !self.saved && self.frames % SAVE_POLL == 0 {
            let title = m.title_id;
            if stable_save(&m.system().services.fs.user_dir.join(format!("savedata/{title:016X}/main")), &mut self.stamp) {
                log::log("character: the first save is on disk: the game's look changes are kept from now on");
                self.saved();
            }
        }
        // a soft reset goes back to the title (once as it comes up); battles, Fly and the party menu unload DllField too,
        // but never load DllTitle. Nothing is written while the title is up
        let title = m.system().cro.modules.iter().any(|d| d.name == "DllTitle");
        if title && !self.title {
            self.rearm();
        }
        self.title = title;
        if title {
            return None;
        }
        let data = field_data(m);
        let mem = &mut m.system().memory;
        let field = data.and_then(|d| field::appearance(&mut Mem(mem), d));
        // writes only once DllField is loaded: X checks the save at boot (MyStatus written then fails the check, and X
        // starts a new game) and has loaded it by then; the player is built a few frames after the field's look shows
        if (self.seen.is_some() && !self.restore) || data.is_none() {
            return self.step(None, field);
        }
        if self.gamedata.is_none() {
            self.gamedata = find_gamedata(mem);
            if let Some(gd) = self.gamedata {
                log::log(&format!("character: GameData at 0x{gd:08X} (frame {})", self.frames));
            }
        }
        // a GameData that is no longer one (a state loaded from another run) is looked for again
        let ms = self.gamedata.and_then(|gd| field::mystatus_of(&mut Mem(mem), gd));
        if ms.is_none() {
            self.gamedata = None;
        }
        let changed = self.step(ms.and_then(|a| mem.slice_mut(a, MS_LEN as u32)), field);
        if let (Some(g), Some(gd)) = (self.clothes, self.gamedata) {
            self.clothes = None;
            CLOTHES.store(u64::from(gd + GD_CLOTHES) << 1 | u64::from(g), Ordering::Relaxed);
            log::log(&format!("character: the intro picked the other gender: the {}'s starting clothes are asked for", gender(g)));
        }
        changed
    }
}

/// code.bin 0x43582C(block, is_girl): the intro's filler of a gender's starting clothes (DllIntro code+0x82FC calls it
/// with GameData+0x1058): the owned-item bits and six outfit slots at +0x10C4.
pub const FILL_CLOTHES: u32 = 0x0043_582C;
/// GameData + this: the starting-clothes block `FILL_CLOTHES` fills, and its length (to GameData+0x1120). The filler
/// only sets bits, so the hook takes the intro gender's fill off and puts the character's on: a girl over the boy's
/// intro then equals a girl's intro byte for byte (checked against Task 8's dumps).
const GD_CLOTHES: u32 = 0x1058;
pub const CLOTHES_LEN: usize = 0xC8;
/// A fill the per-frame hook makes on the game's thread: block << 1 | is_girl; 0 for none.
static CLOTHES: AtomicU64 = AtomicU64::new(0);

/// The hook's side: the fill asked for, once (block, is_girl).
pub fn take_clothes() -> Option<(u32, u32)> {
    let v = CLOTHES.swap(0, Ordering::Relaxed);
    (v != 0).then(|| ((v >> 1) as u32, (v & 1) as u32))
}

/// The character being played: its `LookSync`, and where a look the game changes is kept.
pub struct Played {
    sync: LookSync,
    data: std::path::PathBuf,
    id: u64,
}

impl Played {
    /// `user`: the character's saves folder; a new game when it has no save the player made for `title` yet. None for
    /// a look out of the game's ranges (characters.json's loader drops those).
    pub fn new(c: &redex_proto::Character, data: &std::path::Path, user: &std::path::Path, title: u64) -> Option<Played> {
        let new_game = !played_save(&user.join(format!("savedata/{title:016X}/main")));
        let sync = LookSync::new(Appearance::from_bytes(&c.look)?, &c.name, new_game);
        Some(Played { sync, data: data.to_path_buf(), id: c.id })
    }

    /// After each frame: `LookSync::frame`, and a look the game changed into characters.json. The character as kept
    /// then (for the account), else None.
    pub fn frame(&mut self, m: &mut exec::Machine) -> Option<redex_proto::Character> {
        use xr_frontend::start::characters::{now_ms, Store};
        let look = self.sync.frame(m)?;
        let kept = match Store::try_load(&self.data) {
            None => Err("characters.json can't be read now".into()),
            Some(mut store) => match store.set_look(self.id, look, now_ms()) {
                true => store.save(&self.data).map(|()| store),
                false => Err("it is no longer in characters.json".into()),
            },
        };
        match kept {
            Ok(store) => store.get(self.id).cloned(),
            Err(e) => {
                log::log(&format!("character {:016x}: the new look was not kept: {e}", self.id));
                None
            }
        }
    }
}

/// A save the player made, written whole: `played_save`, with the same length and mtime as at the last poll (`stamp`,
/// updated here), so a file the game is still writing does not count yet.
fn stable_save(main: &std::path::Path, stamp: &mut Option<(u64, Option<std::time::SystemTime>)>) -> bool {
    let now = std::fs::metadata(main).ok().map(|m| (m.len(), m.modified().ok()));
    let still = now.is_some() && now == *stamp;
    *stamp = now;
    still && played_save(main)
}

/// A save the player made: `main` with a trainer name. X writes a nameless `main` at its first boot's language select,
/// long before the intro gives the player a name.
fn played_save(main: &std::path::Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let mut first = [0u8; 2];
    let read = std::fs::File::open(main).and_then(|mut f| {
        f.seek(SeekFrom::Start(SAVE_MYSTATUS + u64::from(MS_NAME)))?;
        f.read_exact(&mut first)
    });
    read.is_ok() && first != [0, 0]
}

fn gender(g: u8) -> &'static str {
    if g == 0 { "boy" } else { "girl" }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

/// GameData: a `GAMEDATA_VTABLE` word in its region whose `+GD_MYSTATUS` points at a MyStatus.
pub fn find_gamedata(mem: &mut xr_hle::memory::Memory) -> Option<u32> {
    const PAGE: u32 = 0x1000;
    let vtable = field::GAMEDATA_VTABLE.to_le_bytes();
    let mut found = Vec::new();
    for page in GAMEDATA_REGION.step_by(PAGE as usize) {
        let Some(s) = mem.slice_mut(page, PAGE) else { continue };
        found.extend(s.chunks_exact(4).enumerate().filter(|(_, w)| *w == vtable).map(|(i, _)| page + 4 * i as u32));
    }
    found.into_iter().find(|&gd| field::mystatus_of(&mut Mem(mem), gd).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use xr_net::x::look::Look;

    /// the 100% save's boy (save 0x14030) and the creator's girl
    const TOBIN: [u8; 16] = [0x88, 0xB9, 0x19, 0x05, 0xD0, 0x09, 0x32, 0x03, 0x11, 0, 0, 0, 0, 0, 0, 0];

    fn boy() -> Appearance {
        Look { gender: 0, skin: 2, hair_style: 3, hair_colour: 1, eyes: 2 }.appearance().unwrap()
    }
    fn girl() -> Appearance {
        Look { gender: 1, skin: 3, hair_style: 4, hair_colour: 2, eyes: 3 }.appearance().unwrap()
    }

    /// MyStatus as the intro leaves it: this gender with its sprite bytes, outfit, and name in both name fields
    fn mystatus(gender: u8, fashion: [u8; 16], name: &str) -> Vec<u8> {
        let mut ms = vec![0xEE; MS_LEN];
        ms[MS_GENDER as usize] = gender;
        ms[MS_SPRITE as usize..][..2].fill(if gender == 0 { 1 } else { 0 });
        ms[MS_FASHION as usize..][..16].copy_from_slice(&fashion);
        for (i, u) in name.encode_utf16().chain(std::iter::repeat(0)).take(NAME_UNITS).enumerate() {
            for at in [MS_NAME, MS_NICKNAME] {
                ms[at as usize + 2 * i..][..2].copy_from_slice(&u.to_le_bytes());
            }
        }
        ms
    }

    #[test]
    fn a_girl_over_a_boys_intro_gets_her_sprite_bytes_and_nickname() {
        let mut s = LookSync::new(girl(), "Mira", true);
        let mut ms = mystatus(0, TOBIN, "Red");
        assert_eq!(&ms[MS_SPRITE as usize..][..2], &[1, 1]);
        s.step(Some(&mut ms), None);
        assert_eq!(&ms[MS_SPRITE as usize..][..2], &[0, 0], "the girl's");
        assert_eq!(&ms[MS_NICKNAME as usize..][..4], &[b'M', 0, b'i', 0]);
        assert_eq!(ms, mystatus(1, girl().fashion, "Mira"));
        // the boy's bytes for a boy; Continue leaves both alone
        let mut s = LookSync::new(boy(), "Calem", true);
        let mut ms = mystatus(1, TOBIN, "Serena");
        s.step(Some(&mut ms), None);
        assert_eq!(ms, mystatus(0, boy().fashion, "Calem"));
        let mut s = LookSync::new(boy(), "Calem", false);
        let mut ms = mystatus(0, TOBIN, "Tobin");
        ms[MS_SPRITE as usize..][..2].copy_from_slice(&[0, 1]);
        ms[MS_NICKNAME as usize] = b'L';
        let before = ms.clone();
        s.step(Some(&mut ms), None);
        assert_eq!((&ms[..MS_FASHION as usize], &ms[MS_NAME as usize..]), (&before[..MS_FASHION as usize], &before[MS_NAME as usize..]));
    }

    #[test]
    fn the_starting_clothes_are_refilled_only_over_the_other_genders_intro() {
        // a girl over a boy intro: once, at the first write
        let mut s = LookSync::new(girl(), "Mira", true);
        let mut ms = mystatus(0, TOBIN, "Red");
        s.step(Some(&mut ms), None);
        assert_eq!(s.clothes.take(), Some(1));
        s.step(Some(&mut ms), None);
        assert_eq!(s.clothes, None, "once");
        // the same gender: the intro's clothes are hers already
        let mut s = LookSync::new(girl(), "Mira", true);
        s.step(Some(&mut mystatus(1, TOBIN, "Serena")), None);
        assert_eq!(s.clothes, None);
        // Continue: never
        let mut s = LookSync::new(boy(), "Calem", false);
        s.step(Some(&mut mystatus(0, TOBIN, "Tobin")), None);
        assert_eq!(s.clothes, None);
    }

    #[test]
    fn new_game_writes_gender_name_and_look_until_the_field() {
        let mut s = LookSync::new(girl(), "Mira", true);
        // the intro's boy "Red", picked in the game: every frame until the field it becomes the character
        let mut ms = mystatus(0, Look::default_for(0).appearance().unwrap().fashion, "Red");
        assert_eq!(s.step(None, None), None, "no GameData yet: nothing to write");
        assert_eq!(s.step(Some(&mut ms), None), None);
        assert_eq!(ms, mystatus(1, girl().fashion, "Mira"));
        ms = mystatus(0, TOBIN, "Red");
        s.step(Some(&mut ms), None);
        assert_eq!(ms, mystatus(1, girl().fashion, "Mira"));
        // the field shows it: no more writes
        assert_eq!(s.step(Some(&mut ms), Some(girl())), None);
        let mut later = mystatus(0, TOBIN, "Red");
        s.step(Some(&mut later), Some(girl()));
        assert_eq!(later, mystatus(0, TOBIN, "Red"));
        // a 12-unit name keeps its terminator
        let mut s = LookSync::new(girl(), "ABCDEFGHIJKL", true);
        s.step(Some(&mut ms), None);
        assert_eq!(ms, mystatus(1, girl().fashion, "ABCDEFGHIJKL"));
    }

    #[test]
    fn continue_writes_only_the_fashion() {
        let mut s = LookSync::new(boy(), "Calem", false);
        let mut ms = mystatus(0, TOBIN, "Tobin");
        s.step(Some(&mut ms), None);
        assert_eq!(ms, mystatus(0, boy().fashion, "Tobin"));
        assert_eq!(s.step(Some(&mut ms), Some(boy())), None);
    }

    #[test]
    fn continue_skips_a_save_of_the_other_gender() {
        let mut s = LookSync::new(girl(), "Mira", false);
        let mut ms = mystatus(0, TOBIN, "Tobin");
        for _ in 0..3 {
            assert_eq!(s.step(Some(&mut ms), None), None);
            assert_eq!(ms, mystatus(0, TOBIN, "Tobin"), "touched nothing");
        }
        let jorge = Appearance { gender: 0, fashion: TOBIN };
        assert_eq!(s.step(Some(&mut ms), Some(jorge)), None);
        // the boy's outfits in that save never become the girl character's
        let mut other = jorge;
        other.fashion[0] ^= 0x40;
        assert!(other.validate());
        assert_eq!(s.step(Some(&mut ms), Some(other)), None);
        assert_eq!(ms, mystatus(0, TOBIN, "Tobin"));
    }

    #[test]
    fn after_the_field_a_change_is_reported_once() {
        // features unlike the save trainer's (his are skin 2, hair style 3, colour 1, eyes 2)
        let boy = || Look { gender: 0, skin: 1, hair_style: 0, hair_colour: 0, eyes: 0 }.appearance().unwrap();
        let mut s = LookSync::new(boy(), "Calem", false);
        s.step(None, Some(boy()));
        assert_eq!(s.step(None, Some(boy())), None, "the same look is no change");
        let jorge = Appearance { gender: 0, fashion: TOBIN };
        assert_eq!(s.step(None, Some(jorge)), Some(jorge));
        assert_eq!(s.step(None, Some(jorge)), None, "once");
        assert_eq!(s.step(None, None), None, "out of the field (a battle) is no change");
        assert_eq!(s.step(None, Some(jorge)), None);
        assert_eq!(s.step(None, Some(boy())), Some(boy()), "back again is a change");
    }

    #[test]
    fn an_invalid_game_look_is_not_reported() {
        let mut s = LookSync::new(boy(), "Calem", false);
        s.step(None, Some(boy()));
        let bad = Appearance { gender: 0, fashion: [0xFF; 16] };
        assert_eq!(s.step(None, Some(bad)), None);
        assert_eq!(s.step(None, Some(Appearance { gender: 7, ..boy() })), None);
        // the last valid look is still the one compared with
        assert_eq!(s.step(None, Some(boy())), None);
    }

    #[test]
    fn a_nameless_main_is_no_save_yet() {
        let main = std::env::temp_dir().join(format!("redex-played-save-{}", std::process::id()));
        let _ = std::fs::remove_file(&main);
        assert!(!played_save(&main), "no file");
        // what the language select writes: MyStatus with no name
        let mut b = vec![0u8; 0x65600];
        b[0x14005] = 1;
        std::fs::write(&main, &b).unwrap();
        assert!(!played_save(&main));
        b[0x14048] = b'M';
        std::fs::write(&main, &b).unwrap();
        assert!(played_save(&main));
        let _ = std::fs::remove_file(&main);
    }

    #[test]
    fn intro_outfits_are_not_taken_before_the_first_save() {
        let mut s = LookSync::new(girl(), "Mira", true);
        let mut ms = mystatus(0, TOBIN, "Red");
        s.step(Some(&mut ms), None);
        s.step(Some(&mut ms), Some(girl()));
        // the pyjamas, then Mom's mirror: the intro's outfits, not the character's
        let pyjamas = Appearance { gender: 1, fashion: [0x40, 0x04, 0x24, 0x02, 0x4C, 0x05, 0x90, 0x02, 0x00, 0x04, 0x20, 0, 0, 0, 0, 0] };
        assert!(pyjamas.validate());
        assert_eq!(s.step(None, Some(pyjamas)), None);
        let mirror = Look::default_for(1).appearance().unwrap();
        assert_eq!(s.step(None, Some(mirror)), None);
        // the first save: a change after it is the character's (a boutique)
        s.saved();
        assert_eq!(s.step(None, Some(pyjamas)), None, "nothing is taken while the restore waits for MyStatus");
        let mut ms = mystatus(1, mirror.fashion, "Mira");
        assert_eq!(s.step(Some(&mut ms), Some(mirror)), None, "what was worn at the save is no change");
        assert_eq!(ms, mystatus(1, girl().fashion, "Mira"), "the character's features on the mirror's clothes");
        assert_eq!(s.step(None, Some(pyjamas)), Some(pyjamas));
    }

    /// Mira (skin 3, hair style 4, colour 2, eyes 3) before and after Mom's mirror, measured in the game: the mirror set
    /// the hair style to 2 and kept the rest.
    const MIRROR: [u8; 16] = [0xC0, 0x3A, 0x24, 0x02, 0x00, 0x20, 0x13, 0x03, 0x41, 0x05, 0x10, 0, 0, 0, 0, 0];

    #[test]
    fn first_save_restores_the_characters_hair_over_the_mirror() {
        let mira = girl();
        assert_eq!(hex(&mira.to_bytes()), "060101c03a4402002013034105100000000000");
        let mirror = Appearance { gender: 1, fashion: MIRROR };
        assert_eq!(Look::of(&mirror), Look { hair_style: 2, ..Look::of(&mira) });
        let mut s = LookSync::new(mira, "Mira", true);
        s.step(Some(&mut mystatus(0, TOBIN, "Red")), None);
        s.step(None, Some(mira));
        // the mirror, in an intro bag: not kept
        let mut intro = mirror;
        intro.fashion[8] = 0x42;
        assert!(intro.validate());
        assert_eq!(s.step(None, Some(intro)), None);
        // the first save: one write of the character's skin, hair and eyes, the intro's clothes left on
        s.saved();
        let mut ms = mystatus(1, intro.fashion, "Mira");
        assert_eq!(s.step(Some(&mut ms), Some(intro)), None);
        let back = Look::of(&mira).wearing(intro.fashion).unwrap();
        assert_eq!((Look::of(&back), back.fashion[8]), (Look::of(&mira), 0x42));
        assert_eq!(ms, mystatus(1, back.fashion, "Mira"));
        // which the field then shows: no change; and only once
        assert_eq!(s.step(Some(&mut mystatus(1, intro.fashion, "Mira")), Some(back)), None);
        let mut again = mystatus(1, intro.fashion, "Mira");
        s.step(Some(&mut again), Some(back));
        assert_eq!(again, mystatus(1, intro.fashion, "Mira"), "written once");
    }

    #[test]
    fn a_salon_change_before_the_restore_is_kept() {
        let mira = girl();
        let mirror = Appearance { gender: 1, fashion: MIRROR };
        let mut s = LookSync::new(mira, "Mira", true);
        s.step(Some(&mut mystatus(0, TOBIN, "Red")), None);
        s.step(None, Some(mira));
        s.step(None, Some(mirror));
        s.saved();
        // between the save and the restore: a salon (hair style 6, colour 0) and a boutique bag
        let mut salon = Look { hair_style: 6, hair_colour: 0, ..Look::of(&mirror) }.wearing(MIRROR).unwrap();
        salon.fashion[8] = 0x42;
        assert!(salon.validate());
        assert_eq!(s.step(None, Some(salon)), None, "waits for MyStatus");
        let mut ms = mystatus(1, salon.fashion, "Mira");
        // the salon's hair and the bag are the player's and kept; skin and eyes, untouched since the mirror, the character's
        // (the same here)
        assert_eq!(s.step(Some(&mut ms), Some(salon)), Some(salon));
        assert_eq!(ms, mystatus(1, salon.fashion, "Mira"));
        // the mirror's hair alone would have gone back to the character's
        let mut s = LookSync::new(mira, "Mira", true);
        s.step(Some(&mut mystatus(0, TOBIN, "Red")), None);
        s.step(None, Some(mirror));
        s.saved();
        let mut ms = mystatus(1, MIRROR, "Mira");
        assert_eq!(s.step(Some(&mut ms), Some(mirror)), None);
        assert_eq!(ms, mystatus(1, mira.fashion, "Mira"));
    }

    #[test]
    fn a_save_still_being_written_does_not_count_yet() {
        let main = std::env::temp_dir().join(format!("redex-stable-save-{}", std::process::id()));
        let _ = std::fs::remove_file(&main);
        let mut stamp = None;
        assert!(!stable_save(&main, &mut stamp), "no file");
        // the game writes MyStatus (with the name) first, the rest after
        let mut b = vec![0u8; 0x20000];
        b[0x14048] = b'M';
        std::fs::write(&main, &b).unwrap();
        assert!(!stable_save(&main, &mut stamp), "just appeared");
        b.resize(0x65600, 0);
        std::fs::write(&main, &b).unwrap();
        assert!(!stable_save(&main, &mut stamp), "still growing");
        assert!(stable_save(&main, &mut stamp), "unchanged since the last poll");
        let _ = std::fs::remove_file(&main);
    }

    #[test]
    fn salon_changes_after_the_first_save_are_kept() {
        let mira = girl();
        let mut s = LookSync::new(mira, "Mira", false);
        s.step(Some(&mut mystatus(1, mira.fashion, "Mira")), None);
        s.step(None, Some(mira));
        // a Lumiose salon: hair style 6, colour 0; then contacts of another colour
        let salon = Look { hair_style: 6, hair_colour: 0, ..Look::of(&mira) }.wearing(mira.fashion).unwrap();
        assert_eq!(s.step(None, Some(salon)), Some(salon));
        let lenses = Look { eyes: 5, ..Look::of(&salon) }.wearing(salon.fashion).unwrap();
        assert_eq!(s.step(None, Some(lenses)), Some(lenses));
        // the other gender never
        assert_eq!(s.step(None, Some(Appearance { gender: 0, fashion: TOBIN })), None);
    }
    #[test]
    fn a_soft_reset_writes_the_look_again_not_a_report() {
        let mut s = LookSync::new(boy(), "Calem", false);
        let mut ms = mystatus(0, TOBIN, "Tobin");
        s.step(Some(&mut ms), None);
        s.step(None, Some(boy()));
        // a battle: the field goes and comes back; nothing to write or report
        assert_eq!(s.step(None, None), None);
        assert_eq!(s.step(None, Some(boy())), None);
        // a soft reset without saving: the save's older look again, written over before the field
        s.rearm();
        let mut ms = mystatus(0, TOBIN, "Tobin");
        assert_eq!(s.step(Some(&mut ms), None), None);
        assert_eq!(ms, mystatus(0, boy().fashion, "Tobin"));
        assert_eq!(s.step(None, Some(boy())), None, "a write, not a report");
    }

    #[test]
    fn a_soft_reset_in_the_intro_writes_gender_and_name_again() {
        let mut s = LookSync::new(girl(), "Mira", true);
        let mut ms = mystatus(0, TOBIN, "Red");
        s.step(Some(&mut ms), None);
        s.step(None, Some(girl()));
        // the intro again: the boy "Red" is the character again
        s.rearm();
        let mut ms = mystatus(0, Look::default_for(0).appearance().unwrap().fashion, "Red");
        s.step(Some(&mut ms), None);
        assert_eq!(ms, mystatus(1, girl().fashion, "Mira"));
        // after the first save a reset writes the outfit only
        s.step(None, Some(girl()));
        s.saved();
        s.rearm();
        let mut ms = mystatus(1, TOBIN, "Mia");
        s.step(Some(&mut ms), None);
        assert_eq!(ms, mystatus(1, girl().fashion, "Mia"));
    }

    #[test]
    fn applied_is_logged_only_when_written_and_shown() {
        let s = LookSync::new(boy(), "Calem", false);
        let jorge = Appearance { gender: 0, fashion: TOBIN };
        assert_eq!(s.verdict(boy()), None, "the save had it already: nothing was written");
        assert!(s.verdict(jorge).unwrap().starts_with("character: look not applied: the field shows 06010088b9"));
        assert!(s.verdict(girl()).unwrap().contains("the save is a girl, the character a boy"));
        let mut s = LookSync::new(girl(), "Mira", true);
        s.step(Some(&mut mystatus(0, TOBIN, "Red")), None);
        assert_eq!(s.verdict(girl()).unwrap(), format!("character: look applied {} (new game)", hex(&girl().to_bytes())));
    }
}
