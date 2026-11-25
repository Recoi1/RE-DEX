//! Room codes: what a lobby's host reads out to friends ("K7M-Q2X"). Six characters from an alphabet without the ones
//! people mix up (no 0/O, 1/I/L, U), five of them random and the last a check character (Luhn mod 30), so a typo
//! or two neighbours swapped is caught before anything is asked of Steam. The code names a Steam lobby whose data holds
//! the host's `steam:<id>` (xr-net `find_room`); it is a way to find the world, not a key to it (the world's door is).

/// The characters a code is made of (30).
pub const ALPHABET: &[u8; 30] = b"23456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Characters in a code, the check character included.
pub const LEN: usize = 6;

/// Why typed text is not a room code (what the player reads).
pub const BAD_LENGTH: &str = "A room code has 6 letters and digits";
pub const BAD_CHAR: &str = "Room codes have no 0, O, 1, I, L or U, and only letters and digits";
pub const TYPO: &str = "That room code has a typo: check it with the host";

fn value(c: u8) -> Option<u32> {
    ALPHABET.iter().position(|&a| a == c).map(|p| p as u32)
}

/// Luhn mod 30 over `digits` (values), the rightmost doubled first when `check_included` is false.
fn luhn_sum(digits: &[u32], check_included: bool) -> u32 {
    let n = ALPHABET.len() as u32;
    let mut double = !check_included;
    digits.iter().rev().fold(0, |sum, &d| {
        let a = if double { d * 2 } else { d };
        double = !double;
        sum + a / n + a % n
    })
}

/// A new code from `random` (any 64 random bits): five characters of it, then the check character.
pub fn new(random: u64) -> String {
    let n = ALPHABET.len() as u64;
    let digits: Vec<u32> = (0..LEN as u32 - 1).map(|i| (random / n.pow(i) % n) as u32).collect();
    let check = (30 - luhn_sum(&digits, false) % 30) % 30;
    digits.iter().chain([&check]).map(|&d| ALPHABET[d as usize] as char).collect()
}

/// What the player typed as a code: spaces and dashes dropped, any case. Ok(the code, upper case), or why it is not one.
pub fn parse(typed: &str) -> Result<String, &'static str> {
    let code: String = typed.chars().filter(|c| !c.is_whitespace() && *c != '-').collect::<String>().to_ascii_uppercase();
    if !code.is_ascii() {
        return Err(BAD_CHAR);
    }
    let digits: Option<Vec<u32>> = code.bytes().map(value).collect();
    match digits {
        _ if code.len() != LEN => Err(BAD_LENGTH),
        None => Err(BAD_CHAR),
        Some(d) if luhn_sum(&d, true) % 30 != 0 => Err(TYPO),
        Some(_) => Ok(code),
    }
}

/// Typed text that is meant as a room code rather than a world address (`steam:<id>`, `host:port`).
pub fn looks_like_code(typed: &str) -> bool {
    !typed.contains(':') && !typed.contains('.')
}

/// How a code is shown: two halves, "K7M-Q2X".
pub fn shown(code: &str) -> String {
    match code.get(..3).zip(code.get(3..)) {
        Some((a, b)) if code.len() == LEN => format!("{a}-{b}"),
        _ => code.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_use_the_unambiguous_alphabet_and_parse_back() {
        assert_eq!(ALPHABET.len(), 30);
        for bad in b"01OILU" {
            assert!(!ALPHABET.contains(bad), "{}", *bad as char);
        }
        let mut seen = std::collections::HashSet::new();
        for i in 0..2000u64 {
            let code = new(i.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            assert_eq!(code.len(), LEN);
            assert!(code.bytes().all(|c| ALPHABET.contains(&c)), "{code}");
            assert_eq!(parse(&code), Ok(code.clone()));
            seen.insert(code);
        }
        assert!(seen.len() > 1990, "codes spread over the space");
    }

    #[test]
    fn case_spaces_and_dashes_do_not_matter() {
        let code = new(123_456_789);
        let lower = code.to_lowercase();
        assert_eq!(parse(&lower), Ok(code.clone()));
        assert_eq!(parse(&shown(&code)), Ok(code.clone()));
        assert_eq!(parse(&format!("  {} {} ", &code[..3], &code[3..].to_lowercase())), Ok(code.clone()));
        assert_eq!(shown(&code), format!("{}-{}", &code[..3], &code[3..]));
    }

    #[test]
    fn typos_are_caught() {
        let code = new(42);
        let b = code.as_bytes();
        // every single wrong character
        for i in 0..LEN {
            for &c in ALPHABET.iter().filter(|&&c| c != b[i]) {
                let mut t = b.to_vec();
                t[i] = c;
                assert_eq!(parse(std::str::from_utf8(&t).unwrap()), Err(TYPO), "{} for {code}", String::from_utf8_lossy(&t));
            }
        }
        // two neighbours swapped (Luhn misses only one pair of values; count what slips through)
        let mut missed = 0;
        for r in 0..300u64 {
            let c = new(r * 7919);
            for i in 0..LEN - 1 {
                let mut t = c.as_bytes().to_vec();
                if t[i] == t[i + 1] {
                    continue;
                }
                t.swap(i, i + 1);
                missed += usize::from(parse(std::str::from_utf8(&t).unwrap()).is_ok());
            }
        }
        assert!(missed * 50 < 300 * (LEN - 1), "{missed} swaps slipped through");
    }

    #[test]
    fn wrong_length_and_lookalikes_say_why() {
        assert_eq!(parse(""), Err(BAD_LENGTH));
        assert_eq!(parse("ABCDE"), Err(BAD_LENGTH));
        assert_eq!(parse("ABCDEFG"), Err(BAD_LENGTH));
        assert_eq!(parse("ABC0EF"), Err(BAD_CHAR), "a zero");
        assert_eq!(parse("ABCOEF"), Err(BAD_CHAR), "an O");
        assert_eq!(parse("AB1IEL"), Err(BAD_CHAR));
        assert_eq!(parse("ABCD\u{c9}F"), Err(BAD_CHAR));
        assert!(looks_like_code("k7m-q2x") && !looks_like_code("steam:1") && !looks_like_code("127.0.0.1:7777"));
    }
}
