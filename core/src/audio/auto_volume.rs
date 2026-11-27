//! Decides how the macOS input volume should move so speech reaches the
//! microphone at a steady level, like Tuple's and Zoom's "automatically adjust
//! microphone volume". WebRTC's digital gain then only fine-tunes.
//!
//! It looks at the raw microphone signal in 10 ms frames. Every two seconds it
//! takes the frames that stand out from the background noise as speech, and if
//! their level is far from the target it asks for a small volume step: up
//! slowly, down faster, and down at once when the signal clips.

/// Frames per decision: 2 s of 10 ms frames.
const WINDOW_FRAMES: usize = 200;
/// Speech level to aim for, as the RMS of a 10 ms frame (peaks land ~12 dB higher).
const TARGET_DBFS: f32 = -26.0;
/// No step while speech is within this distance of the target.
const DEADBAND_DB: f32 = 4.0;
/// Largest steps per decision.
const MAX_STEP_UP_DB: f32 = 2.0;
const MAX_STEP_DOWN_DB: f32 = 4.0;
/// Step down when this many frames in a window hit full scale.
const CLIPPED_FRAMES_LIMIT: usize = 3;
const CLIP_STEP_DB: f32 = -4.0;
/// Speech must be at least this loud, and this far above the window's noise floor.
const SPEECH_FLOOR_DBFS: f32 = -50.0;
const SPEECH_ABOVE_NOISE_DB: f32 = 15.0;
/// At least this much speech (0.4 s) is needed in a window to judge its level.
const MIN_SPEECH_FRAMES: usize = 40;
/// A sample this close to full scale counts as clipping.
const CLIP_LEVEL: i16 = 32_000;

pub struct AutoVolume {
    frame_dbfs: Vec<f32>,
    clipped_frames: usize,
}

impl Default for AutoVolume {
    fn default() -> Self {
        Self {
            frame_dbfs: Vec::with_capacity(WINDOW_FRAMES),
            clipped_frames: 0,
        }
    }
}

fn frame_dbfs(frame: &[i16]) -> f32 {
    if frame.is_empty() {
        return f32::NEG_INFINITY;
    }
    let sum_sq: f64 = frame.iter().map(|&s| (s as f64) * (s as f64)).sum();
    let rms = (sum_sq / frame.len() as f64).sqrt() / i16::MAX as f64;
    (20.0 * rms.max(1e-9).log10()) as f32
}

impl AutoVolume {
    /// Takes one 10 ms frame of the raw microphone signal. Every two seconds
    /// returns the volume change in dB that would bring speech to the target,
    /// or `None` when no change is needed.
    pub fn push_frame(&mut self, frame: &[i16]) -> Option<f32> {
        self.frame_dbfs.push(frame_dbfs(frame));
        if frame.iter().any(|&s| s >= CLIP_LEVEL || s <= -CLIP_LEVEL) {
            self.clipped_frames += 1;
        }
        if self.frame_dbfs.len() < WINDOW_FRAMES {
            return None;
        }
        let step = self.decide();
        self.frame_dbfs.clear();
        self.clipped_frames = 0;
        step
    }

    fn decide(&self) -> Option<f32> {
        if self.clipped_frames >= CLIPPED_FRAMES_LIMIT {
            return Some(CLIP_STEP_DB);
        }

        let mut sorted = self.frame_dbfs.clone();
        sorted.sort_by(f32::total_cmp);
        let noise_floor = sorted[sorted.len() / 10];
        let speech_threshold = SPEECH_FLOOR_DBFS.max(noise_floor + SPEECH_ABOVE_NOISE_DB);
        let speech: Vec<f32> = sorted
            .into_iter()
            .filter(|&db| db >= speech_threshold)
            .collect();
        if speech.len() < MIN_SPEECH_FRAMES {
            return None;
        }

        // Median speech level: robust against a cough or a door slam.
        let speech_dbfs = speech[speech.len() / 2];
        let error = TARGET_DBFS - speech_dbfs;
        if error.abs() <= DEADBAND_DB {
            return None;
        }
        Some(error.clamp(-MAX_STEP_DOWN_DB, MAX_STEP_UP_DB))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_LEN: usize = 480;

    /// A 10 ms frame of a sine at `dbfs` RMS.
    fn tone(dbfs: f32) -> Vec<i16> {
        let amplitude = 10f32.powf(dbfs / 20.0) * std::f32::consts::SQRT_2 * i16::MAX as f32;
        (0..FRAME_LEN)
            .map(|i| (amplitude * (i as f32 * 0.13).sin()) as i16)
            .collect()
    }

    /// Two seconds where `speech_frames` frames are speech at `speech_dbfs`
    /// over background noise at -65 dBFS.
    fn window(speech_dbfs: f32, speech_frames: usize) -> Option<f32> {
        let mut auto_volume = AutoVolume::default();
        let mut result = None;
        for i in 0..WINDOW_FRAMES {
            let level = if i < speech_frames {
                speech_dbfs
            } else {
                -65.0
            };
            result = auto_volume.push_frame(&tone(level));
        }
        result
    }

    #[test]
    fn raises_quiet_speech_slowly() {
        assert_eq!(window(-40.0, 120), Some(MAX_STEP_UP_DB));
    }

    #[test]
    fn lowers_loud_speech() {
        let step = window(-14.0, 120).expect("loud speech should be turned down");
        assert!(step < 0.0 && step >= -MAX_STEP_DOWN_DB, "step {step}");
    }

    #[test]
    fn leaves_speech_near_the_target_alone() {
        assert_eq!(window(-27.0, 120), None);
    }

    #[test]
    fn ignores_windows_without_speech() {
        assert_eq!(window(-65.0, 0), None);
        assert_eq!(window(-40.0, 10), None);
    }

    #[test]
    fn steps_down_when_clipping() {
        let mut auto_volume = AutoVolume::default();
        let mut result = None;
        for i in 0..WINDOW_FRAMES {
            let frame = if i < 5 {
                vec![i16::MAX; FRAME_LEN]
            } else {
                tone(-65.0)
            };
            result = auto_volume.push_frame(&frame);
        }
        assert_eq!(result, Some(CLIP_STEP_DB));
    }

    #[test]
    fn decides_once_per_window() {
        let mut auto_volume = AutoVolume::default();
        let decisions = (0..WINDOW_FRAMES * 3)
            .filter_map(|i| {
                let level = if i % WINDOW_FRAMES < 120 {
                    -40.0
                } else {
                    -65.0
                };
                auto_volume.push_frame(&tone(level))
            })
            .count();
        assert_eq!(decisions, 3);
    }
}
