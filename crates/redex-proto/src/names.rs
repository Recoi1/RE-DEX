//! Names players see: what no player may be called (the world's display names and the backend's usernames share it).

/// Names nobody may show (they pass for the server's staff or the game's makers), compared `fold`ed, with only
/// letters and digits kept.
pub const RESERVED: [&str; 8] = ["admin", "moderator", "redex", "nintendo", "gamefreak", "pokemon", "system", "server"];

/// A character as `clean_name` compares it: full-width forms as ASCII, lower case, and common look-alikes (digits,
/// symbols, Cyrillic) as the letter they pass for.
pub fn fold(c: char) -> char {
    let c = match c as u32 {
        0xFF01..=0xFF5E => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
        _ => c,
    };
    match c.to_lowercase().next().unwrap_or(c) {
        '0' => 'o',
        '1' | 'l' | '|' | '!' => 'i',
        '3' | 'é' | 'è' | 'ê' => 'e',
        '4' | '@' => 'a',
        '5' | '$' => 's',
        '7' => 't',
        'а' => 'a',
        'е' => 'e',
        'о' => 'o',
        'р' => 'p',
        'с' => 'c',
        'х' => 'x',
        'у' => 'y',
        'і' => 'i',
        'ѕ' => 's',
        'к' => 'k',
        'м' => 'm',
        'т' => 't',
        c => c,
    }
}

/// Whether `name` passes for staff or the game's makers (or has no letter or digit at all): folded, only letters and
/// digits kept, then compared with `RESERVED`.
pub fn reserved(name: &str) -> bool {
    let key: String = name.chars().map(fold).filter(|c| c.is_alphanumeric()).collect();
    key.is_empty() || RESERVED.contains(&key.as_str())
}

#[cfg(test)]
mod tests {
    #[test]
    fn reserved_names_see_through_case_spacing_and_look_alikes() {
        for n in ["Admin", " a d m i n ", "RE:DEX", "Game Freak", "Pok\u{e9}mon", "\u{FF21}\u{FF24}\u{FF2D}\u{FF29}\u{FF2E}", "\u{0410}dmin", "M0derat0r", "", "--"] {
            assert!(super::reserved(n), "{n:?}");
        }
        for n in ["Serena", "Adminton", "Ash_99"] {
            assert!(!super::reserved(n), "{n:?}");
        }
    }
}