//! The interface's own sounds (assets/sounds, made by tools/ui-sounds/make_ui_sounds.py, CC0): a click on a press, a
//! chime up when a panel opens and down when it closes, a bright one for Play, a low one for a refusal, a ping with a
//! toast. Played on their own small output stream, opened the first time one plays, only once the app turned them on
//! (`set_enabled`, from Configure > Audio's Interface sounds); tests never turn them on, so they never open a device.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sound {
    Click,
    Open,
    Close,
    Confirm,
    Refuse,
    Toast,
}

static ENABLED: AtomicBool = AtomicBool::new(false);
static MIXER: OnceLock<Option<Mixer>> = OnceLock::new();

/// what plays: each voice's samples and where it is in them
struct Mixer {
    voices: Arc<Mutex<Vec<(Arc<[f32]>, f64)>>>,
}

/// interface sounds on or off (Configure > Audio); never in the test harness's off-screen windows
pub fn set_enabled(on: bool) {
    static HARNESS: OnceLock<bool> = OnceLock::new();
    let harness = *HARNESS.get_or_init(|| std::env::var_os("XR_TEST_OFFSCREEN").is_some());
    // unit tests never make a sound
    ENABLED.store(on && !harness && !cfg!(test), Ordering::Relaxed);
}

/// plays `sound` when the sounds are on
pub fn play(sound: Sound) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let Some(mixer) = MIXER.get_or_init(open).as_ref() else { return };
    let samples = samples(sound);
    if let Ok(mut v) = mixer.voices.lock() {
        // a burst of the same click never piles up
        v.retain(|(s, pos)| !(Arc::ptr_eq(s, &samples) && *pos < 2400.0));
        if v.len() < 8 {
            v.push((samples, 0.0));
        }
    }
}

/// plays Open or Close when `open` differs from the last time this `id` was seen (a panel's state, once a frame)
pub fn edge(ctx: &egui::Context, id: egui::Id, open: bool) {
    edge_with(ctx, id, open, Some(Sound::Open), Some(Sound::Close));
}

/// plays `on` as the state turns true and `off` as it turns false
pub fn edge_with(ctx: &egui::Context, id: egui::Id, state: bool, on: Option<Sound>, off: Option<Sound>) {
    let was = ctx.data(|d| d.get_temp::<bool>(id));
    ctx.data_mut(|d| d.insert_temp(id, state));
    if let Some(s) = was.filter(|w| *w != state).and(if state { on } else { off }) {
        play(s);
    }
}

fn samples(sound: Sound) -> Arc<[f32]> {
    static DECODED: OnceLock<Vec<Arc<[f32]>>> = OnceLock::new();
    let all = DECODED.get_or_init(|| {
        [
            &include_bytes!("../../../assets/sounds/click.wav")[..],
            include_bytes!("../../../assets/sounds/open.wav"),
            include_bytes!("../../../assets/sounds/close.wav"),
            include_bytes!("../../../assets/sounds/confirm.wav"),
            include_bytes!("../../../assets/sounds/refuse.wav"),
            include_bytes!("../../../assets/sounds/toast.wav"),
        ]
        .iter()
        .map(|b| decode(b).into())
        .collect()
    });
    all[sound as usize].clone()
}

/// a 16-bit mono WAV's samples (our own files: the `data` chunk found by name)
fn decode(bytes: &[u8]) -> Vec<f32> {
    let at = bytes.windows(4).position(|w| w == b"data").map_or(bytes.len(), |p| p + 8);
    bytes[at.min(bytes.len())..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect()
}

/// the default output at its own rate, mixing what plays; the stream lives on a thread of its own (it is not Send
/// on every host), parked for the program's life
fn open() -> Option<Mixer> {
    let voices: Arc<Mutex<Vec<(Arc<[f32]>, f64)>>> = Arc::new(Mutex::new(Vec::new()));
    let mix = voices.clone();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("ui sounds".into())
        .spawn(move || match stream(mix) {
            Some(s) => {
                let _ = tx.send(true);
                loop {
                    std::thread::park();
                    let _ = &s;
                }
            }
            None => {
                let _ = tx.send(false);
            }
        })
        .ok()?;
    rx.recv().ok()?.then_some(Mixer { voices })
}

/// 48 kHz sounds, stepped to the device's rate and mixed
fn stream(mix: Arc<Mutex<Vec<(Arc<[f32]>, f64)>>>) -> Option<cpal::Stream> {
    let device = cpal::default_host().default_output_device()?;
    let config = device.default_output_config().ok()?;
    let channels = config.channels() as usize;
    let step = 48_000.0 / config.sample_rate().0 as f64;
    let stream = device
        .build_output_stream(
            &config.into(),
            move |out: &mut [f32], _| {
                out.fill(0.0);
                let Ok(mut v) = mix.lock() else { return };
                for frame in out.chunks_mut(channels) {
                    let mut s = 0.0;
                    for (samples, pos) in v.iter_mut() {
                        let i = *pos as usize;
                        if i + 1 < samples.len() {
                            let f = (*pos - i as f64) as f32;
                            s += samples[i] * (1.0 - f) + samples[i + 1] * f;
                        }
                        *pos += step;
                    }
                    frame.fill((s * 0.6).clamp(-1.0, 1.0));
                }
                v.retain(|(samples, pos)| (*pos as usize) + 1 < samples.len());
            },
            |e| log::warn!("interface sounds: {e}"),
            None,
        )
        .ok()?;
    stream.play().ok()?;
    Some(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_sound_decodes_short_and_audible() {
        for s in [Sound::Click, Sound::Open, Sound::Close, Sound::Confirm, Sound::Refuse, Sound::Toast] {
            let v = samples(s);
            assert!(v.len() > 1000 && v.len() < 48_000, "{s:?}: {} samples", v.len());
            assert!(v.iter().any(|x| x.abs() > 0.3), "{s:?} is silent");
        }
        // off by default: playing opens nothing
        play(Sound::Click);
        assert!(MIXER.get().is_none());
    }
}
