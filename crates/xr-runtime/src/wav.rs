//! A stereo 16-bit WAV written as the run goes, so a long run's sound is never all in memory. The header's sizes are
//! put in by `finish`; a run that stops early leaves them at their most (players read such a file to its end).

use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub struct Wav {
    out: std::io::BufWriter<std::fs::File>,
    path: PathBuf,
    /// samples (stereo pairs) so far, those not silent, the loudest
    pub samples: u64,
    loud: u64,
    peak: u16,
}

impl Wav {
    pub fn create(path: &Path) -> Result<Wav, String> {
        let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut wav = Wav { out: std::io::BufWriter::new(file), path: path.to_owned(), samples: 0, loud: 0, peak: 0 };
        wav.out.write_all(&header(u32::MAX - 36)).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(wav)
    }

    pub fn push(&mut self, sound: &[[i16; 2]]) {
        self.samples += sound.len() as u64;
        self.loud += sound.iter().filter(|s| s != &&[0, 0]).count() as u64;
        self.peak = sound.iter().flatten().map(|s| s.unsigned_abs()).fold(self.peak, u16::max);
        let bytes: Vec<u8> = sound.iter().flatten().flat_map(|s| s.to_le_bytes()).collect();
        if let Err(e) = self.out.write_all(&bytes) {
            crate::log::log(&format!("{}: {e}", self.path.display()));
        }
    }

    /// the sizes in the header, and the log's `<what>: N samples, M not silent, peak P` line
    pub fn finish(&mut self, what: &str) {
        crate::log::log(&format!("{what}: {} samples, {} not silent, peak {}", self.samples, self.loud, self.peak));
        let data = (self.samples * 4).min(u32::MAX as u64 - 36) as u32;
        let done = self.out.flush().and_then(|()| self.out.get_mut().seek(SeekFrom::Start(0))).and_then(|_| self.out.get_mut().write_all(&header(data)));
        if let Err(e) = done {
            crate::log::log(&format!("{}: {e}", self.path.display()));
        }
    }
}

/// RIFF/WAVE, PCM, stereo, 16 bits, the DSP's rate rounded (32728 Hz for 32728.5), `data` bytes of samples
fn header(data: u32) -> Vec<u8> {
    let rate = xr_hle::AUDIO_SAMPLE_RATE.round() as u32;
    let mut h = Vec::with_capacity(44);
    for part in [&b"RIFF"[..], &(36 + data).to_le_bytes(), b"WAVEfmt ", &16u32.to_le_bytes(), &1u16.to_le_bytes(), &2u16.to_le_bytes(), &rate.to_le_bytes(), &(rate * 4).to_le_bytes(), &4u16.to_le_bytes(), &16u16.to_le_bytes(), b"data", &data.to_le_bytes()] {
        h.extend_from_slice(part);
    }
    h
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_finished_wav_says_how_long_it_is() {
        let path = std::env::temp_dir().join(format!("xr-wav-test-{}.wav", std::process::id()));
        let mut w = super::Wav::create(&path).unwrap();
        w.push(&[[1, -2], [0, 0]]);
        w.push(&[[300, -32768]]);
        w.finish("audio");
        let got = (w.samples, w.loud, w.peak);
        drop(w);
        let b = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(b.len(), 44 + 12);
        assert_eq!(&b[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(b[4..8].try_into().unwrap()), 36 + 12);
        assert_eq!(u32::from_le_bytes(b[24..28].try_into().unwrap()), 32728);
        assert_eq!(u32::from_le_bytes(b[40..44].try_into().unwrap()), 12);
        assert_eq!(&b[44..48], &[1, 0, 0xFE, 0xFF]);
        assert_eq!(got, (3, 2, 32768));
    }
}
