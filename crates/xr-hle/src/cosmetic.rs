//! The RomFS files of Pokémon X and Y a mod may replace and still play online: archives (GARCs) holding only the
//! models, textures and animations of characters and Pokémon, and the menus' pictures and layouts. Anything else a mod
//! replaces (stats, moves, encounters, trainers, items, text, scripts, maps, the CROs, code) keeps it offline.
//!
//! Found from the game's own files (X 1.0, USA; Y lays its RomFS out the same, and the mods for both use these
//! paths): every GARC of the RomFS was read, each entry LZ11-unpacked, and what it holds told apart by its magic
//! (BCH, CGFX, CLIM, the two-letter packs around them) and by the model names inside. Each entry says what it held.

/// the game's characters and Pokémon, each GARC checked by hand
const MODELS: [(&str, &str); 5] = [
    ("a/0/0/7", "Pokémon models: a small index table, then 7,851 packs (PC, PT, PB, PK) of BCH and CGFX models, textures and animations; what Pokémon skins replace"),
    ("a/0/2/1", "field models of trainers and people: 286 MM packs of BCH models and their tables (tr0157_fi_00_body, ToonTable)"),
    ("a/0/8/8", "the player's model parts: 7 small tables of which part an outfit draws, then 600+ BCH models (hair, socks); what player skins replace"),
    ("a/0/8/9", "the other gender's player model parts: 29 tables, then 700+ BCH models (skirts, tops); what player skins replace"),
    ("a/1/1/2", "outfit icons: 266 BCLIM pictures and one table; clothing skins replace them with the models"),
];

/// the menus' and screens' pictures and layouts: GARCs whose every entry is a BCLIM picture, or a layout archive
/// (bclyt, bclan, bclim) that ends with one (a/0/0/5 holds the battle menu's, a/1/4/5 the PSS walls')
const PICTURES: [&str; 94] = [
    "a/0/0/0", "a/0/0/4", "a/0/0/5", "a/0/0/8", "a/0/1/0", "a/0/1/1", "a/0/1/4", "a/0/1/5", "a/0/1/6", "a/0/1/8",
    "a/0/2/7", "a/0/2/9", "a/0/3/0", "a/0/3/5", "a/0/4/3", "a/0/4/4", "a/0/4/5", "a/0/4/6", "a/0/4/7", "a/0/4/8",
    "a/0/4/9", "a/0/5/0", "a/0/5/1", "a/0/5/2", "a/0/5/3", "a/0/5/4", "a/0/5/5", "a/0/5/6", "a/0/5/7", "a/0/5/8",
    "a/0/5/9", "a/0/6/3", "a/0/6/7", "a/0/6/9", "a/0/7/0", "a/0/7/1", "a/0/9/1", "a/0/9/3", "a/0/9/4", "a/0/9/5",
    "a/0/9/6", "a/0/9/7", "a/0/9/8", "a/0/9/9", "a/1/0/0", "a/1/0/4", "a/1/1/1", "a/1/1/4", "a/1/1/7", "a/1/1/8",
    "a/1/1/9", "a/1/4/1", "a/1/4/5", "a/1/5/0", "a/1/5/3", "a/1/5/4", "a/1/5/8", "a/1/5/9", "a/1/6/0", "a/1/6/1",
    "a/1/6/2", "a/1/6/3", "a/1/6/6", "a/1/7/0", "a/1/7/2", "a/1/7/6", "a/1/7/8", "a/1/7/9", "a/1/8/6", "a/1/9/0",
    "a/1/9/3", "a/1/9/8", "a/2/0/0", "a/2/0/8", "a/2/0/9", "a/2/2/2", "a/2/2/3", "a/2/2/5", "a/2/2/6", "a/2/2/7",
    "a/2/2/8", "a/2/2/9", "a/2/3/1", "a/2/3/2", "a/2/3/4", "a/2/4/6", "a/2/4/9", "a/2/5/2", "a/2/5/4", "a/2/5/7",
    "a/2/6/0", "a/2/6/2", "a/2/6/7", "a/2/7/0",
];

/// is the RomFS file at path (from the RomFS's top, either slash, any case) one a mod may replace and stay online
pub fn cosmetic(path: &str) -> bool {
    let path = path.replace('\\', "/").to_ascii_lowercase();
    let path = path.trim_start_matches('/');
    MODELS.iter().any(|(p, _)| *p == path) || PICTURES.contains(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_and_pictures_are_cosmetic_and_game_data_is_not() {
        for path in ["a/0/0/7", "A/0/8/8", "a\\0\\8\\9", "/a/1/1/2", "a/0/0/5", "a/2/7/0"] {
            assert!(cosmetic(path), "{path}");
        }
        // personal data, moves, trainers, encounters, maps, text (a/0/7/4 is English, a clothing skin renames
        // outfits there), story text, the battle AI, the CROs, the code
        for path in ["a/2/1/8", "a/2/1/2", "a/0/3/8", "a/0/4/0", "a/0/1/2", "a/0/1/3", "a/0/4/1", "a/0/7/4", "a/0/8/0", "btl_ai.garc", "DllField.cro", "static.crs", "a/0/0/7/0327.bin", "a/0/0", "code.bin", ""] {
            assert!(!cosmetic(path), "{path}");
        }
    }

    #[test]
    fn the_list_has_each_garc_once() {
        let mut all: Vec<&str> = MODELS.iter().map(|(p, _)| *p).chain(PICTURES).collect();
        assert!(all.iter().all(|p| p.len() == 7 && p.starts_with("a/") && p.bytes().skip(2).step_by(2).all(|b| b.is_ascii_digit())));
        all.sort();
        let n = all.len();
        all.dedup();
        assert_eq!(all.len(), n);
    }
}
