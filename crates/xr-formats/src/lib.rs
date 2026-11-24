pub mod exheader;
pub mod functab;
pub use functab::{parse_functab, write_functab, FuncEntry, Mode};
pub use exheader::{parse_exheader, Exheader, Segment};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatError {
    TooShort { need: usize, got: usize },
    BadMagic { what: &'static str },
    Other(String),
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort { need, got } => write!(f, "too short: need {need} bytes, got {got}"),
            Self::BadMagic { what } => write!(f, "bad magic: {what}"),
            Self::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for FormatError {}
