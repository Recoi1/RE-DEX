//! XR_AUDIO_NO_BGM (headless captures): the game's background music left out of what the speakers play, its sound
//! effects, cries, fanfares and ambience kept. Pokémon X and Y stream their music as AAC files the DSP decodes:
//! RomFS `sound/bgm_*.aac` the background music, `sound/me_*.aac` the fanfares (level up, items, badges); everything
//! else plays from the sound archive's own waves. The game reads its RomFS as one file and copies each AAC frame to
//! one input buffer for the decoder, so the music is told by its bytes: the music file they are found in (looked for
//! where that file's last frame ended, else in every music file) says whether the PCM the decoder writes is background
//! music, and a voice playing from that PCM is left out. Host side only: what the title reads back (statuses, the
//! final mix) stays the same, so the game runs the same with it on or off, and it works from a saved state at once.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};

/// on for the whole process: a loaded state keeps it
static NO_BGM: AtomicBool = AtomicBool::new(false);

pub fn set_no_bgm(on: bool) {
    NO_BGM.store(on, Ordering::Relaxed);
}

pub fn no_bgm() -> bool {
    NO_BGM.load(Ordering::Relaxed)
}

/// the music a file's bytes are, by its RomFS path
pub fn is_bgm_file(path: &str) -> Option<bool> {
    let name = path.rsplit(['/', '\\']).next()?.to_ascii_lowercase();
    name.ends_with(".aac").then(|| name.starts_with("bgm_"))
}

/// address ranges and whether the music in them is background music, a newer write over an older one
#[derive(Default)]
pub struct Tags(BTreeMap<u32, (u32, bool)>);

impl Tags {
    /// [start, start + length) now holds bgm (Some) or something else (None)
    pub fn set(&mut self, start: u32, length: u32, bgm: Option<bool>) {
        let end = start.saturating_add(length);
        if length == 0 {
            return;
        }
        // the ranges overlapping it are cut back to what lies outside it
        let overlapping: Vec<(u32, (u32, bool))> = self.0.range(..end).rev().take_while(|(_, (e, _))| *e > start).map(|(s, v)| (*s, *v)).collect();
        for (s, (e, b)) in overlapping {
            self.0.remove(&s);
            if s < start {
                self.0.insert(s, (start, b));
            }
            if e > end {
                self.0.insert(end, (e, b));
            }
        }
        if let Some(b) = bgm {
            self.0.insert(start, (end, b));
        }
    }

    /// what the byte at address holds, if music
    pub fn at(&self, address: u32) -> Option<bool> {
        self.0.range(..=address).next_back().filter(|(_, (end, _))| address < *end).map(|(_, (_, b))| *b)
    }
}

/// a music file: where its bytes are in the image, and whether it is background music
pub struct Track {
    pub name: String,
    pub range: std::ops::Range<usize>,
    pub bgm: bool,
}

/// the PCM the decoder made of music (by the physical address voices play from), the music files, and where each
/// one's next frame is expected
#[derive(Default)]
pub struct Music {
    pub pcm: Tags,
    pub tracks: Option<Vec<Track>>,
    next: HashMap<usize, usize>,
    /// decodes left before the next search of every file, after one found nothing
    cooldown: u32,
}

impl Music {
    /// the music files of a RomFS read thru a window at base in image (none if it doesn't parse)
    pub fn tracks_of(image: &[u8], base: u64) -> Vec<Track> {
        let Ok(fs) = zakuro_fs::RomFs::parse_level3(image, base) else { return Vec::new() };
        let mut out = Vec::new();
        let mut dirs = fs.root().map(|r| vec![r]).unwrap_or_default();
        while let Some(dir) = dirs.pop() {
            dirs.extend(fs.subdirs(&dir).into_iter().map(|(_, d)| d));
            for (_, f) in fs.files(&dir) {
                if let Some(bgm) = is_bgm_file(&f.name) {
                    let start = fs.file_data_offset(&f) as usize;
                    out.push(Track { range: start..start + f.data_size as usize, bgm, name: f.name });
                }
            }
        }
        out
    }

    /// whose music an AAC frame (or a run of them) is: Some(true) background music, Some(false) a fanfare, None not
    /// found in any music file (image: where the tracks' bytes are)
    pub fn of(&mut self, image: &[u8], data: &[u8]) -> Option<bool> {
        let tracks = self.tracks.as_ref()?;
        let needle = &data[..data.len().min(64)];
        if needle.len() < 32 {
            return None;
        }
        let bytes = |i: usize| image.get(tracks[i].range.clone()).unwrap_or(&[]);
        let hinted = self.next.iter().find(|&(&i, &at)| bytes(i).get(at..).is_some_and(|b| b.starts_with(needle))).map(|(&i, &at)| (i, at));
        let found = hinted.or_else(|| {
            if self.cooldown > 0 {
                self.cooldown -= 1;
                return None;
            }
            // a new track (or the first frame after a saved state): every file, a payload byte first
            let hit = (0..tracks.len()).find_map(|i| bytes(i).windows(needle.len()).position(|w| w[31] == needle[31] && w == needle).map(|at| (i, at)));
            if hit.is_none() {
                self.cooldown = 100;
            }
            hit
        });
        let (i, at) = found?;
        self.next.insert(i, at + data.len());
        Some(tracks[i].bgm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_music_is_told_from_fanfares_by_file_name() {
        assert_eq!(is_bgm_file("rom:/sound/bgm_xy_r_a.aac"), Some(true));
        assert_eq!(is_bgm_file("/sound/BGM_XY_WIN1.AAC"), Some(true));
        assert_eq!(is_bgm_file("/sound/me_xy_lvup.aac"), Some(false));
        assert_eq!(is_bgm_file("/sound/xy_sound.bcsar"), None);
        assert_eq!(is_bgm_file("/a/0/1/2"), None);
    }

    #[test]
    fn a_frame_is_the_music_of_the_file_it_comes_from() {
        let image: Vec<u8> = (0..10000u32).map(|i| (i * 7 + i / 251 + i / 13) as u8).collect();
        let track = |name: &str, range, bgm| Track { name: name.into(), range, bgm };
        let mut m = Music { tracks: Some(vec![track("bgm_xy_r_a.aac", 1000..5000, true), track("me_xy_lvup.aac", 6000..9000, false)]), ..Default::default() };
        assert_eq!(m.of(&image, &image[2000..2300]), Some(true));
        assert_eq!(m.next[&0], 1300, "the next frame is looked for where this one ended");
        assert_eq!(m.of(&image, &image[2300..2500]), Some(true));
        assert_eq!(m.of(&image, &image[6010..6400]), Some(false));
        assert_eq!(m.of(&image, &[1; 64]), None);
        assert_eq!((m.cooldown, m.of(&image, &image[7000..7100])), (100, None), "no search right after one found nothing");
        assert_eq!(m.of(&image, &image[100..400]), None, "not in a music file");
    }

    #[test]
    fn a_newer_write_replaces_what_it_covers() {
        let mut t = Tags::default();
        t.set(100, 100, Some(true));
        assert_eq!((t.at(99), t.at(100), t.at(199), t.at(200)), (None, Some(true), Some(true), None));
        // a fanfare over the middle, then something else over its start
        t.set(140, 20, Some(false));
        assert_eq!((t.at(139), t.at(140), t.at(159), t.at(160)), (Some(true), Some(false), Some(false), Some(true)));
        t.set(130, 15, None);
        assert_eq!((t.at(129), t.at(130), t.at(144), t.at(145)), (Some(true), None, None, Some(false)));
        // one write over several
        t.set(0, 1000, Some(false));
        assert_eq!(t.0.len(), 1);
        assert_eq!(t.at(500), Some(false));
    }
}
