//! Playthrough scripts: `FRAME NAME [HOLD]` presses (see `press`), `FRAME checkpoint NAME` (save both screens and hash
//! them after that frame), `expect NAME HASH` (the hash that checkpoint must have) and `autobattle [X,Y] [heal X,Y;X,Y;... PCT]
//! [pp X,Y;X,Y;... N] [then X,Y N]` (battles play themselves: touch move X,Y; while the player's HP bar is below PCT% the heal touches,
//! 150 frames apart, from the command screen (a heal move: FIGHT then the move; an item: BAG, pocket, item, USE, the
//! Pokemon); the pp touches the same way every N move picks (a Max Elixir); the `then` move after N uses of the first, when its PP run out: see main.rs).
//! `faint X,Y;X,Y;... N`: after N move picks with no command screen between (the lead fainted and the "Battle using
//! which Pokemon?" list is up), go back to the list and send out the next of these party slots; with `faint` there is
//! also no A on the move screen. `alt X,Y N`: move X,Y after N-1 turns of the main move (`alt 240,60 2` alternates two
//! moves, which Torment forces). `flee`: RUN from each battle; one still on after 20 s (a trainer's) is fought (and, as
//! with `faint`, no A on the move screen: it would pick the cursor's move; blind heals only when the last reading was
//! below PCT+30; with neither the command nor the move screen for 30 s, a touch on back; after 6 picks without the
//! command screen, back, and the `then` move for the rest of that battle: the main move's PP ran out). After a `faint`
//! send-out the `then` move is used too, on these lines (the next Pokemon's). `mon SPECIES[;SPECIES]`: the
//! HP comes from the battle's record of the lead (species, max HP, HP, level; see main.rs `hp_mem`), not the HP bar,
//! which some battle cameras hide; `mon` lines (fleeing or not) take the same newer rules as `flee`.
//! `catch SPECIES[;SPECIES] X,Y;...` (with `mon`): a wild one of these species gets the ball touches instead of a fight
//! or RUN (again while it breaks free), then B (no nickname); one catch per run.
//! `FRAME wait_until COND [TIMEOUT]` stops the script clock at FRAME until COND holds (checked
//! after every frame; TIMEOUT frames, default 3600, then exit 4), so a networked script waits on the other instance
//! instead of on frame numbers. COND is `puppets OP N`, `map OP N` (both need the field), `ADDR OP VALUE` (the u32 at a hex
//! address, XR_PEEK's syntax) or `hash OP HASH` (both screens' checkpoint hash); OP is = != >= <= > <. A press that
//! covers FRAME stays held while it waits (`100 UP 2` then `101 wait_until map!=0` walks up until the map changes), so
//! keep other presses off FRAME. A checkpoint at FRAME is taken when the wait starts: put it at FRAME+1 for after.
//! `#` starts a comment.
use super::{Press, press};
use sha2::{Digest, Sha256};

/// An `autobattle` line.
#[derive(Debug, PartialEq, Clone)]
pub struct Auto {
    pub mv: (u16, u16),
    pub heal: Option<(Vec<(u16, u16)>, u32)>,
    pub pp: Option<(Vec<(u16, u16)>, u32)>,
    pub then: Option<((u16, u16), u32)>,
    /// the party slots to send out, in turn, when the lead has fainted (N move picks with no command screen between)
    pub faint: Option<(Vec<(u16, u16)>, u32)>,
    /// a move after N-1 turns of the main one (2: alternate, as Torment forces)
    pub alt: Option<((u16, u16), u32)>,
    /// `flee`: RUN from each battle; one still on after 20 s (a trainer's, which refuses RUN) is fought
    pub flee: bool,
    /// `mon SPECIES[;SPECIES...]`: read the lead's HP from the battle's Pokemon record in memory instead of the HP bar
    /// (several: an evolution changes the species)
    pub mon: Vec<u16>,
    /// `catch SPECIES[;SPECIES] X,Y;X,Y;...`: a wild battle with one of these species (read from the battle's record
    /// of the foe, needs `mon`) gets the touches (Bag, the Poke Balls pocket, the ball, USE) instead of a fight or RUN,
    /// again while it breaks free, and B for the rest of that battle (no nickname); one catch per run
    pub catch: Option<(Vec<u16>, Vec<(u16, u16)>)>,
}

impl Auto {
    /// `flee` and `mon` lines take the newer rules (Normal first moves, no A on the move screen, backing out of stray
    /// screens); older scripts keep the old ones, and so their hashes.
    pub fn newer(&self) -> bool {
        self.flee || !self.mon.is_empty()
    }
}

/// A `wait_until` line.
#[derive(Debug, PartialEq, Clone)]
pub struct Wait {
    pub at: u64,
    pub what: What,
    pub op: Op,
    /// frames until the run gives up (exit 4)
    pub timeout: u64,
    /// the line as written, for the log
    pub text: String,
}

/// What a `wait_until` reads.
#[derive(Debug, PartialEq, Clone)]
pub enum What {
    /// puppets in the field's pool (ids from 0x8000)
    Puppets(u64),
    /// the player's map (field snapshot)
    Map(u64),
    /// (address, value): the u32 there
    Word(u32, u64),
    /// both screens' checkpoint hash
    Hash(String),
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Op {
    Eq,
    Ne,
    Ge,
    Le,
    Gt,
    Lt,
}

impl Op {
    pub fn holds<T: PartialOrd>(self, a: T, b: T) -> bool {
        match self {
            Op::Eq => a == b,
            Op::Ne => a != b,
            Op::Ge => a >= b,
            Op::Le => a <= b,
            Op::Gt => a > b,
            Op::Lt => a < b,
        }
    }
}

/// `puppets>=1`, `map!=0x41`, `818290C=0`, `hash=0123abcd...` (spaces around OP are allowed: the tokens are joined).
fn condition(c: &str) -> Option<(What, Op)> {
    let ops = [("!=", Op::Ne), (">=", Op::Ge), ("<=", Op::Le), ("=", Op::Eq), (">", Op::Gt), ("<", Op::Lt)];
    let (i, len, op) = ops.iter().find_map(|&(s, op)| c.find(s).map(|i| (i, s.len(), op)))?;
    let (l, r) = (c[..i].trim(), c[i + len..].trim());
    let num = |v: &str| match v.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => v.parse().ok(),
    };
    let what = match l {
        "puppets" => What::Puppets(num(r)?),
        "map" => What::Map(num(r)?),
        "hash" if matches!(op, Op::Eq | Op::Ne) && !r.is_empty() && r.chars().all(|c| c.is_ascii_hexdigit()) => What::Hash(r.to_lowercase()),
        a => What::Word(u32::from_str_radix(a.trim_start_matches("0x"), 16).ok()?, num(r)?),
    };
    Some((what, op))
}

#[derive(Debug, PartialEq)]
pub struct Checkpoint {
    pub at: u64,
    pub name: String,
    pub expect: Option<String>,
}

pub type Script = (Vec<Press>, Vec<Checkpoint>, Option<Auto>, Vec<Wait>);

pub fn parse(text: &str) -> Result<Script, String> {
    let (mut presses, mut cps, mut expects, mut auto, mut waits) = (Vec::new(), Vec::new(), Vec::new(), None, Vec::new());
    for l in text.lines().map(|l| l.split('#').next().unwrap().trim()).filter(|l| !l.is_empty()) {
        let w: Vec<&str> = l.split_whitespace().collect();
        match w[..] {
            ["expect", name, hash] => expects.push((name, hash)),
            ["autobattle", ref rest @ ..] => auto = Some(autobattle(rest).ok_or(format!("\"{l}\": autobattle [X,Y] [heal X,Y;... PCT] [pp X,Y;... N] [then X,Y N] [alt X,Y N] [faint X,Y;... N]"))?),
            [at, "wait_until", ref rest @ ..] if !rest.is_empty() => {
                let bad = || format!("\"{l}\": FRAME wait_until puppets|map|ADDR|hash OP VALUE [TIMEOUT]");
                let at = at.parse::<u64>().ok().filter(|&a| a > 0).ok_or_else(bad)?;
                // a last word that is a plain number after a whole condition is the timeout
                let (cond, timeout) = match rest {
                    [c @ .., t] if !c.is_empty() && condition(&c.concat()).is_some() => (c.concat(), t.parse().map_err(|_| bad())?),
                    c => (c.concat(), 3600),
                };
                let (what, op) = condition(&cond).ok_or_else(bad)?;
                waits.push(Wait { at, what, op, timeout, text: cond });
            }
            [at, "checkpoint", name] => {
                let at = at.parse::<u64>().ok().filter(|&a| a > 0).ok_or(format!("\"{l}\": frame must be a number of 1 or more"))?;
                cps.push(Checkpoint { at, name: name.into(), expect: None });
            }
            [at, name] => presses.push(press(&format!("{name}@{at}"))?),
            [at, name, hold] => presses.push(press(&format!("{name}@{at}+{hold}"))?),
            _ => return Err(format!("\"{l}\" is not FRAME NAME [HOLD], FRAME checkpoint NAME or expect NAME HASH")),
        }
    }
    for (name, hash) in expects {
        let cp = cps.iter_mut().find(|c| c.name == name).ok_or(format!("expect {name}: no such checkpoint"))?;
        cp.expect = Some(hash.into());
    }
    cps.sort_by_key(|c| c.at);
    waits.sort_by_key(|w| w.at);
    Ok((presses, cps, auto, waits))
}

/// The move to touch on the move screen; 80,125 (the bottom-left one) by default.
fn autobattle(w: &[&str]) -> Option<Auto> {
    let flee = w.contains(&"flee");
    let w: Vec<&str> = w.iter().copied().filter(|&s| s != "flee").collect();
    let w = &w[..];
    let xy = |s: &str| super::press(&format!("TOUCH:{s}@1")).ok()?.touch;
    let (mv, mut rest) = match w.first() {
        Some(s) if !["heal", "pp", "then", "faint", "alt", "mon", "catch"].contains(s) => (xy(s)?, &w[1..]),
        _ => ((80, 125), w),
    };
    let mut a = Auto { mv, heal: None, pp: None, then: None, faint: None, alt: None, flee, mon: vec![], catch: None };
    loop {
        if let ["mon", n, tail @ ..] = rest {
            a.mon = n.split(';').map(|v| v.parse().ok()).collect::<Option<_>>()?;
            rest = tail;
            continue;
        }
        if let ["catch", sp, touches, tail @ ..] = rest {
            let sp = sp.split(';').map(|v| v.parse().ok()).collect::<Option<_>>()?;
            a.catch = Some((sp, touches.split(';').map(xy).collect::<Option<_>>()?));
            rest = tail;
            continue;
        }
        let [k, p, n, tail @ ..] = rest else { break };
        let n = n.parse().ok()?;
        match *k {
            "heal" => a.heal = Some((p.split(';').map(xy).collect::<Option<_>>()?, n)),
            "pp" => a.pp = Some((p.split(';').map(xy).collect::<Option<_>>()?, n)),
            "faint" => a.faint = Some((p.split(';').map(xy).collect::<Option<_>>()?, n)),
            "then" => a.then = Some((xy(p)?, n)),
            "alt" if n > 0 => a.alt = Some((xy(p)?, n)),
            _ => return None,
        }
        rest = tail;
    }
    rest.is_empty().then_some(a)
}

/// The header line `--record-expects` writes: what the hashes depend on besides the script (a comment to `parse`).
const HEADER: &str = "# expects recorded with: ";

/// What the script's expects were recorded with (its header line), if it says.
pub fn recorded_with(text: &str) -> Option<&str> {
    text.lines().find_map(|l| l.strip_prefix(HEADER)).map(str::trim)
}

/// The script with its `expect` lines replaced by the observed hashes, each right after its checkpoint line, and the
/// header line (first) saying they were recorded with `with` (the DLL build and renderer).
pub fn record(text: &str, seen: &[(String, String)], with: &str) -> String {
    let mut out = format!("{HEADER}{with}\n");
    for l in text.lines() {
        let w: Vec<&str> = l.split('#').next().unwrap().split_whitespace().collect();
        if w.first() == Some(&"expect") || l.starts_with(HEADER) {
            continue;
        }
        out += l;
        out += "\n";
        if let [_, "checkpoint", name] = w[..] {
            if let Some((_, h)) = seen.iter().find(|(n, _)| n == name) {
                out += &format!("expect {name} {h}\n");
            }
        }
    }
    out
}

/// A short hash of the guest's screen bytes (RGBA, so it does not depend on the PNG encoder).
pub fn hash(top: &[u8], bottom: &[u8]) -> String {
    let d = Sha256::new().chain_update(top).chain_update(bottom).finalize();
    d[..8].iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_records() {
        let t = "10 A\n20 UP 3 # x\n30 checkpoint a\nexpect a 00ff\n40 checkpoint b\n";
        let (p, c, auto, w) = parse(t).unwrap();
        assert!(w.is_empty());
        assert_eq!((p.len(), c.len(), auto), (2, 2, None));
        assert_eq!(parse("autobattle\n").unwrap().2, Some(Auto { mv: (80, 125), heal: None, pp: None, then: None, faint: None, alt: None, flee: false, mon: vec![], catch: None }));
        assert!(parse("autobattle flee
").unwrap().2.unwrap().flee);
        let a = parse("autobattle flee heal 1,2 50 mon 653
").unwrap().2.unwrap();
        assert_eq!((a.mon, a.heal.map(|h| h.1)), (vec![653], Some(50)));
        assert_eq!(parse("autobattle heal 1,2 50 mon 653;654
").unwrap().2.unwrap().mon, vec![653, 654]);
        let a = parse("autobattle mon 653 catch 661;25 40,215;240,45 flee
").unwrap().2.unwrap();
        assert_eq!(a.catch, Some((vec![661, 25], vec![(40, 215), (240, 45)])));
        assert!(a.flee);
        let a = Auto { mv: (80, 60), heal: Some((vec![(160, 90), (80, 125)], 45)), pp: None, then: Some(((240, 125), 15)), faint: None, alt: None, flee: false, mon: vec![], catch: None };
        assert_eq!(parse("autobattle 80,60 heal 160,90;80,125 45 then 240,125 15\n").unwrap().2, Some(a));
        assert!(parse("autobattle 80,60 heal 80,125\n").is_err());
        assert_eq!(parse("autobattle pp 1,2;3,4 12\n").unwrap().2.unwrap().pp, Some((vec![(1, 2), (3, 4)], 12)));
        assert_eq!(parse("autobattle faint 240,45;80,95 3\n").unwrap().2.unwrap().faint, Some((vec![(240, 45), (80, 95)], 3)));
        assert_eq!(parse("autobattle alt 240,60 2\n").unwrap().2.unwrap().alt, Some(((240, 60), 2)));
        assert!(parse("autobattle alt 240,60 0\n").is_err());
        assert_eq!(c[0], Checkpoint { at: 30, name: "a".into(), expect: Some("00ff".into()) });
        assert_eq!(c[1].expect, None);
        let r = record(t, &[("a".into(), "11".into()), ("b".into(), "22".into())], "build x, renderer y");
        let body = "10 A\n20 UP 3 # x\n30 checkpoint a\nexpect a 11\n40 checkpoint b\nexpect b 22\n";
        assert_eq!(r, format!("# expects recorded with: build x, renderer y\n{body}"));
        assert_eq!(recorded_with(&r), Some("build x, renderer y"));
        assert_eq!(recorded_with(t), None);
        // recording again replaces the header, and parse ignores it
        let again = record(&r, &[("a".into(), "11".into()), ("b".into(), "22".into())], "build z");
        assert_eq!(again, format!("# expects recorded with: build z\n{body}"));
        assert_eq!(parse(&again).unwrap().1.len(), 2);
        // wait_until: the condition (spaces allowed), an optional timeout
        let w = parse("50 wait_until puppets >= 1 600
10 wait_until map!=0x41
20 wait_until 818290C=0
30 wait_until hash=AB12
").unwrap().3;
        assert_eq!(w.iter().map(|w| w.at).collect::<Vec<_>>(), [10, 20, 30, 50]);
        assert_eq!((&w[3].what, w[3].op, w[3].timeout), (&What::Puppets(1), Op::Ge, 600));
        assert_eq!((&w[0].what, w[0].op, w[0].timeout), (&What::Map(0x41), Op::Ne, 3600));
        assert_eq!(w[1].what, What::Word(0x0818_290C, 0));
        assert_eq!(w[2].what, What::Hash("ab12".into()));
        for bad in ["5 wait_until", "5 wait_until puppets", "5 wait_until hash>1", "5 wait_until puppets=x", "0 wait_until map=1", "5 wait_until map=1 x"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert!(Op::Ge.holds(2, 1) && !Op::Lt.holds(2, 1) && Op::Ne.holds("a", "b"));
        assert!(parse("expect z 1").is_err() && parse("0 checkpoint a").is_err() && parse("1 2 3 4").is_err());
        assert_ne!(hash(&[1], &[2]), hash(&[2], &[1]));
    }
}
