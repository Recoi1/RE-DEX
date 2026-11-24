//! The player's look on the wire: the save's gender and trainer fashion, checked against the ranges the game itself
//! allows. Shared by the client (xr-net, which writes a peer's look into game memory only after this check) and the
//! world server (which stores and relays only looks that pass it).
pub use crate::APPEARANCE_VERSION;

/// `Appearance::to_bytes`: the generation tag and the format version, first.
pub const APPEARANCE_GEN: u8 = 6;
/// gen, version, gender, the 16 fashion bytes
pub const APPEARANCE_LEN: usize = 19;

/// One fashion bit field: (word, first bit, width, min, max).
type Field = (usize, u32, u32, u32, u32);

/// The boy's fields and their ranges, as the game's own check (code.bin 0x16A350) takes them: a value out of range
/// makes the game use its default outfit. Skin (word 0, bits 6-7) is not checked there and goes straight into the
/// dress-up body: only the values seen in saves (1, 2 and 3) pass. Contacts (word 2, bit 0) is a flag.
const BOY: &[Field] = &[
    (0, 0, 3, 0, 0), // must be 0
    (0, 3, 3, 1, 1), // 1 for a boy
    (0, 6, 2, 1, 3), // skin
    (0, 8, 3, 0, 4), // hair colour
    (0, 11, 5, 0, 23), // hat
    (0, 16, 3, 0, 1),
    (0, 19, 4, 0, 4), // hair style
    (0, 23, 3, 0, 4), // face
    (0, 26, 2, 0, 1),
    (0, 28, 2, 0, 1),
    (1, 0, 6, 0, 35),  // top
    (1, 6, 5, 0, 17),  // bottom
    (1, 11, 3, 0, 5),  // socks
    (1, 14, 5, 0, 13), // shoes
    (1, 19, 4, 0, 7),  // bag
    (1, 23, 4, 0, 12), // hat accessory
    (1, 27, 2, 0, 1),
    (2, 0, 1, 0, 1), // contacts
    (2, 1, 3, 0, 2),
    (2, 4, 3, 0, 4), // eye (contact) colour
    (2, 7, 3, 0, 4),
    (2, 10, 4, 0, 7),
    (2, 14, 4, 0, 7),
    (2, 18, 3, 0, 4),
    (2, 21, 3, 0, 4),
    (2, 24, 2, 0, 1),
    (2, 26, 3, 0, 4),
];

/// The girl's (code.bin 0x16A584). Skin and the contacts flag (word 2, bit 10) as for the boy.
const GIRL: &[Field] = &[
    (0, 0, 3, 0, 0),
    (0, 3, 3, 0, 0), // 0 for a girl
    (0, 6, 2, 1, 3), // skin
    (0, 8, 3, 0, 4), // hair colour
    (0, 11, 6, 0, 31), // hat
    (0, 17, 3, 0, 2),
    (0, 20, 4, 0, 6), // hair style
    (0, 24, 3, 0, 4), // face
    (0, 27, 2, 0, 1),
    (0, 29, 2, 0, 1),
    (1, 0, 6, 0, 35),  // top
    (1, 6, 7, 0, 45),  // bottom
    (1, 13, 4, 0, 10), // dress
    (1, 17, 5, 0, 27), // socks
    (1, 22, 6, 0, 27), // shoes
    (1, 28, 2, 0, 1),
    (2, 0, 5, 0, 16), // bag
    (2, 5, 5, 0, 24), // hat accessory
    (2, 10, 1, 0, 1), // contacts
    (2, 11, 2, 0, 2),
    (2, 13, 2, 0, 1),
    (2, 15, 2, 0, 1),
    (2, 17, 2, 0, 1),
    (2, 19, 3, 0, 4), // eye (contact) colour
    (2, 22, 3, 0, 4),
    (2, 25, 3, 0, 4),
    (2, 28, 3, 0, 4),
    (3, 0, 3, 0, 4),
    (3, 3, 2, 0, 1),
    (3, 5, 3, 0, 4),
];

/// What the player looks like: the save's gender and trainer fashion (MyStatus +0x05 and +0x30), as the game keeps them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Appearance {
    /// 0 boy, 1 girl
    pub gender: u8,
    pub fashion: [u8; 16],
}

impl Appearance {
    /// The fashion's four u32 words.
    pub fn words(&self) -> [u32; 4] {
        std::array::from_fn(|i| u32::from_le_bytes(self.fashion[4 * i..4 * i + 4].try_into().unwrap()))
    }

    /// Every field in the range the game's check allows, and every bit the game does not read 0 (a peer's bytes are
    /// written to game memory only after this).
    pub fn validate(&self) -> bool {
        let fields = match self.gender {
            0 => BOY,
            1 => GIRL,
            _ => return false,
        };
        let w = self.words();
        let mut used = [0u32; 4];
        for &(i, at, bits, min, max) in fields {
            let mask = (1u32 << bits) - 1;
            let v = (w[i] >> at) & mask;
            if v < min || v > max {
                return false;
            }
            used[i] |= mask << at;
        }
        (0..4).all(|i| w[i] & !used[i] == 0)
    }

    /// The compact form sent to other players: gen tag, version, gender, the 16 fashion bytes.
    pub fn to_bytes(&self) -> [u8; APPEARANCE_LEN] {
        let mut b = [0; APPEARANCE_LEN];
        b[..3].copy_from_slice(&[APPEARANCE_GEN, APPEARANCE_VERSION, self.gender]);
        b[3..].copy_from_slice(&self.fashion);
        b
    }

    /// The gender in a peer's bytes even when the outfit fails `validate`: byte 2 when the tag, version and length
    /// are this format's and it is 0 or 1; else 0 (the plain boy).
    pub fn gender_of(b: &[u8]) -> u8 {
        match b {
            [APPEARANCE_GEN, APPEARANCE_VERSION, g @ (0 | 1), ..] if b.len() == APPEARANCE_LEN => *g,
            _ => 0,
        }
    }

    /// From a peer's bytes: None unless the tag, version and length are this format's and `validate` holds.
    pub fn from_bytes(b: &[u8]) -> Option<Appearance> {
        if b.len() != APPEARANCE_LEN || b[0] != APPEARANCE_GEN || b[1] != APPEARANCE_VERSION {
            return None;
        }
        let a = Appearance { gender: b[2], fashion: b[3..].try_into().ok()? };
        a.validate().then_some(a)
    }
}
