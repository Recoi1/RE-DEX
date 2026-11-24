//! The character creator's choices and their form in the game's appearance bytes.
use super::field::Appearance;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Look {
    pub gender: u8,
    pub skin: u8,
    pub hair_style: u8,
    pub hair_colour: u8,
    pub eyes: u8,
}

pub const DEFAULT_BOY: [u8; 16] = [0x48, 0x64, 0x09, 0x01, 0x01, 0xcb, 0x08, 0x03, 0x41, 0, 0, 0, 0, 0, 0, 0];
/// The girl's starting outfit after Mom's mirror, measured on the real game.
pub const DEFAULT_GIRL: [u8; 16] = [0x40, 0x3c, 0x24, 0x02, 0x00, 0x20, 0x13, 0x03, 0x41, 0x05, 0x20, 0, 0, 0, 0, 0];
pub const SKINS: [u8; 3] = [1, 2, 3];
pub const HAIR_COLOURS: u8 = 5;
/// 0 natural (contacts off); 1..=5 contacts, colour 0..=4.
pub const EYES: u8 = 6;

pub fn hair_styles(gender: u8) -> u8 {
    if gender == 0 { 5 } else { 7 }
}

/// (hair style bit, contacts flag bit, eye colour bit) for a gender's word 0 / word 2.
fn layout(gender: u8) -> (u32, u32, u32) {
    if gender == 0 { (19, 0, 4) } else { (20, 10, 19) }
}

impl Look {
    pub fn of(a: &Appearance) -> Look {
        let w = a.words();
        let (hs, flag, ec) = layout(a.gender);
        let eyes = if (w[2] >> flag) & 1 == 0 { 0 } else { 1 + ((w[2] >> ec) & 7) as u8 };
        Look {
            gender: a.gender,
            skin: (w[0] >> 6 & 3) as u8,
            hair_colour: (w[0] >> 8 & 7) as u8,
            hair_style: (w[0] >> hs & 15) as u8,
            eyes,
        }
    }

    pub fn default_for(gender: u8) -> Look {
        let fashion = if gender == 0 { DEFAULT_BOY } else { DEFAULT_GIRL };
        Look::of(&Appearance { gender, fashion })
    }

    /// None unless every option is in range and the result passes `Appearance::validate`.
    pub fn appearance(&self) -> Option<Appearance> {
        self.wearing(if self.gender == 0 { DEFAULT_BOY } else { DEFAULT_GIRL })
    }

    /// This look's skin, hair and eyes in the clothes of `outfit` (fashion bytes of this look's gender); None as for
    /// `appearance`.
    pub fn wearing(&self, outfit: [u8; 16]) -> Option<Appearance> {
        if self.gender > 1 || !SKINS.contains(&self.skin) || self.hair_colour >= HAIR_COLOURS
            || self.hair_style >= hair_styles(self.gender) || self.eyes >= EYES
        {
            return None;
        }
        let mut w = Appearance { gender: self.gender, fashion: outfit }.words();
        let (hs, flag, ec) = layout(self.gender);
        w[0] = w[0] & !(3 << 6 | 7 << 8 | 15 << hs) | (self.skin as u32) << 6 | (self.hair_colour as u32) << 8 | (self.hair_style as u32) << hs;
        w[2] &= !(1 << flag | 7 << ec);
        if self.eyes > 0 {
            w[2] |= 1 << flag | ((self.eyes - 1) as u32) << ec;
        }
        let mut fashion = [0u8; 16];
        for i in 0..4 {
            fashion[4 * i..4 * i + 4].copy_from_slice(&w[i].to_le_bytes());
        }
        let a = Appearance { gender: self.gender, fashion };
        a.validate().then_some(a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_option_validates_and_round_trips() {
        for gender in 0..2 {
            for &skin in &SKINS {
                for hair_style in 0..hair_styles(gender) {
                    for hair_colour in 0..HAIR_COLOURS {
                        for eyes in 0..EYES {
                            let look = Look { gender, skin, hair_style, hair_colour, eyes };
                            let a = look.appearance().unwrap_or_else(|| panic!("{look:?}"));
                            assert!(a.validate());
                            assert_eq!(Look::of(&a), look);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn defaults_are_the_games_starting_looks() {
        assert_eq!(Look::default_for(0).appearance().unwrap().fashion, DEFAULT_BOY);
        assert_eq!(Look::default_for(1).appearance().unwrap().fashion, DEFAULT_GIRL);
    }

    #[test]
    fn out_of_range_options_give_none() {
        let b = Look::default_for(0);
        assert!(Look { skin: 0, ..b }.appearance().is_none());
        assert!(Look { hair_style: 5, ..b }.appearance().is_none());
        assert!(Look { eyes: 6, ..b }.appearance().is_none());
        assert!(Look { gender: 2, ..b }.appearance().is_none());
    }
}
