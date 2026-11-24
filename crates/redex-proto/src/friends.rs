//! Friend frames: what one RE:DEX sends another, game to game over Steam's networking messages (not through a world
//! server). A frame is `FRIEND_VERSION (u8), kind (u8)`, then the kind's fields, little endian, at most `FRIEND_MAX`
//! bytes. Anything else (another version, a cut or oversized frame, bad UTF-8, an invite that names no Steam world) does
//! not decode: the receiver drops it.
use crate::{ProtoError, R, W, WHISPER_BYTES};

/// Bumped on any change of the friend frames.
pub const FRIEND_VERSION: u8 = 1;
/// Bytes of a friend frame at most.
pub const FRIEND_MAX: usize = 512;
/// Characters of an invite's world name (a longer one is cut, sending and receiving).
pub const WORLD_CHARS: usize = 32;

#[derive(Debug, Clone, PartialEq)]
pub enum FriendMsg {
    /// One-to-one chat text (the receiver cleans it as a whisper).
    Chat(String),
    /// "Come to my world": its name and `steam:<server id>`.
    Invite { world: String, connect: String },
    /// The invite was turned down.
    Decline,
}

/// `steam:<digits>`: a world by its SteamID, what Steam's "Join game", `+connect` and a friend's invite may name. A
/// `host:port` there could come from anyone and would get our Steam ticket over TCP, unbound to that server.
pub fn steam_world(addr: &str) -> bool {
    addr.strip_prefix("steam:").is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) && id.parse::<u64>().is_ok())
}

fn world_name(s: &str) -> String {
    s.chars().take(WORLD_CHARS).collect()
}

pub fn encode_friend(m: &FriendMsg) -> Vec<u8> {
    let mut w = W(vec![FRIEND_VERSION]);
    match m {
        FriendMsg::Chat(text) => {
            w.u8(1);
            w.long_str(text, WHISPER_BYTES);
        }
        FriendMsg::Invite { world, connect } => {
            w.u8(2);
            w.str(&world_name(world));
            w.str(connect);
        }
        FriendMsg::Decline => w.u8(3),
    }
    w.0
}

pub fn decode_friend(b: &[u8]) -> Option<FriendMsg> {
    if b.len() > FRIEND_MAX {
        return None;
    }
    let mut r = R(b);
    let m = (|| -> Result<Option<FriendMsg>, ProtoError> {
        if r.u8()? != FRIEND_VERSION {
            return Ok(None);
        }
        Ok(match r.u8()? {
            1 => Some(FriendMsg::Chat(r.long_string(WHISPER_BYTES)?)),
            2 => {
                let (world, connect) = (r.string(255)?, r.string(255)?);
                steam_world(&connect).then(|| FriendMsg::Invite { world: world_name(&world), connect })
            }
            3 => Some(FriendMsg::Decline),
            _ => None,
        })
    })()
    .ok()
    .flatten()?;
    r.0.is_empty().then_some(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_each_kind() {
        for m in [
            FriendMsg::Chat("hi \u{e9}".into()),
            FriendMsg::Chat(String::new()),
            FriendMsg::Invite { world: "Kalos Friends".into(), connect: "steam:90297831083063301".into() },
            FriendMsg::Decline,
        ] {
            let b = encode_friend(&m);
            assert!(b.len() <= FRIEND_MAX);
            assert_eq!(decode_friend(&b), Some(m));
        }
        // the longest chat (WHISPER_CHARS characters of 4 bytes) fits a frame
        let t = "\u{1F600}".repeat(crate::WHISPER_CHARS);
        let b = encode_friend(&FriendMsg::Chat(t.clone()));
        assert!(b.len() <= FRIEND_MAX, "{} bytes", b.len());
        assert_eq!(decode_friend(&b), Some(FriendMsg::Chat(t)));
    }

    #[test]
    fn decode_rejects_junk() {
        let good = encode_friend(&FriendMsg::Chat("hello".into()));
        assert_eq!(decode_friend(&[]), None, "empty");
        let mut v = good.clone();
        v[0] = FRIEND_VERSION + 1;
        assert_eq!(decode_friend(&v), None, "another version");
        for n in 1..good.len() {
            assert_eq!(decode_friend(&good[..n]), None, "truncated to {n}");
        }
        let mut trailing = good.clone();
        trailing.push(0);
        assert_eq!(decode_friend(&trailing), None, "trailing byte");
        assert_eq!(decode_friend(&[FRIEND_VERSION, 99]), None, "unknown kind");
        // 64 KB of text: over the frame limit, and a length past the chat limit
        let mut big = vec![FRIEND_VERSION, 1, 0xFF, 0xFF];
        big.extend(vec![b'a'; 0xFFFF]);
        assert_eq!(decode_friend(&big), None, "64 KB");
        let mut over = vec![FRIEND_VERSION, 1];
        over.extend(((crate::WHISPER_BYTES + 1) as u16).to_le_bytes());
        over.extend(vec![b'a'; crate::WHISPER_BYTES + 1]);
        assert!(over.len() <= FRIEND_MAX);
        assert_eq!(decode_friend(&over), None, "text over WHISPER_BYTES");
        let mut bad = vec![FRIEND_VERSION, 1, 2, 0];
        bad.extend([0xC3, 0x28]);
        assert_eq!(decode_friend(&bad), None, "bad UTF-8");
    }

    #[test]
    fn invite_connect_must_be_steam_digits() {
        let invite = |c: &str| decode_friend(&encode_friend(&FriendMsg::Invite { world: "w".into(), connect: c.into() }));
        assert!(invite("steam:123").is_some());
        for bad in ["steam:", "steam:12a", "1.2.3.4:7777", "", " steam:1", "steam:99999999999999999999"] {
            assert_eq!(invite(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn world_name_trimmed_to_32_chars() {
        let long = "\u{e9}".repeat(40);
        let got = decode_friend(&encode_friend(&FriendMsg::Invite { world: long, connect: "steam:1".into() }));
        assert_eq!(got, Some(FriendMsg::Invite { world: "\u{e9}".repeat(WORLD_CHARS), connect: "steam:1".into() }));
        // a frame from another build with a longer name is cut on receipt too
        let mut raw = vec![FRIEND_VERSION, 2, 40];
        raw.extend([b'w'; 40]);
        raw.extend([7]);
        raw.extend(b"steam:1");
        assert_eq!(decode_friend(&raw), Some(FriendMsg::Invite { world: "w".repeat(WORLD_CHARS), connect: "steam:1".into() }));
    }
}
