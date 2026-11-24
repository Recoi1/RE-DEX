//! Known-good builds, identified by whole-file SHA-256.
#[derive(Debug, Clone, Copy)]
pub struct KnownBuild {
    pub id: &'static str,
    pub sha256: &'static str,
    pub title_id: u64,
    pub product_code: &'static str,
}

pub const KNOWN: [KnownBuild; 2] = [
    KnownBuild {
        id: "x-usa-v0.2.0",
        sha256: "4c153a06a684ef0cfa52d57fb270bc64c8779ea1a686e8a4c1360f6491976837",
        title_id: 0x0004000000055D00,
        product_code: "CTR-P-EKJA",
    },
    KnownBuild {
        id: "y-usa-v0.2.0",
        sha256: "0e4278dbfbaf216867ee19922671e8b33a707f13602a5df9d6d5a7210848ba12",
        title_id: 0x0004000000055E00,
        product_code: "CTR-P-EK2A",
    },
];
