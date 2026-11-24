//! Pokemon X (x-usa-v0.2.0).
pub mod field;
pub mod heap;
pub mod look;
pub mod puppet;
pub mod wardrobe;

/// The trainer's name in X's `main` save file: MyStatus (0x14000) + 0x48, 13 UTF-16 units. None when the file is
/// too short or the name is empty.
pub fn trainer_name(main: &[u8]) -> Option<String> {
    let b = main.get(0x14048..0x14048 + 0x1A)?;
    let units: Vec<u16> = b.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
    let name = String::from_utf16_lossy(&units).trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// The trainer's look in X's `main` save file: gender at MyStatus (0x14000) + 0x05 and the fashion at +0x30, the bytes
/// the field reads live (`field::appearance`). None when the file is too short or the gender is neither 0 nor 1; the
/// fashion is not checked here (other players check it, and show the plain look of the gender when it fails). It is
/// what other players get at login (their puppet's model and outfit), before the field is read.
pub fn appearance(main: &[u8]) -> Option<field::Appearance> {
    let a = field::Appearance { gender: *main.get(0x14005)?, fashion: main.get(0x14030..0x14040)?.try_into().ok()? };
    (a.gender <= 1).then_some(a)
}

#[cfg(test)]
mod tests {
    #[test]
    fn trainer_name_from_main() {
        let mut main = vec![0u8; 0x65600];
        for (i, u) in "Serena".encode_utf16().enumerate() {
            main[0x14048 + 2 * i..][..2].copy_from_slice(&u.to_le_bytes());
        }
        assert_eq!(super::trainer_name(&main).as_deref(), Some("Serena"));
        assert_eq!(super::trainer_name(&main[..0x14000]), None);
        assert_eq!(super::trainer_name(&vec![0u8; 0x65600]), None);
    }

    #[test]
    fn appearance_from_main() {
        let mut main = vec![0u8; 0x65600];
        let girl = [0xC0, 0, 0x24, 2, 0x4C, 5, 0x90, 2, 0, 4, 0, 0, 0, 0, 0, 0];
        main[0x14005] = 1;
        main[0x14030..0x14040].copy_from_slice(&girl);
        assert_eq!(super::appearance(&main), Some(super::field::Appearance { gender: 1, fashion: girl }));
        // an outfit that fails the ranges still gives the gender (peers check the outfit); a gender of 7 or a short file
        // gives nothing
        main[0x14005] = 0;
        assert!(super::appearance(&main).is_some_and(|a| a.gender == 0 && !a.validate()));
        main[0x14005] = 7;
        assert_eq!(super::appearance(&main), None);
        assert_eq!(super::appearance(&main[..0x14035]), None);
    }
}
