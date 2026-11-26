use livekit::webrtc::native::apm::AudioProcessingModule;

use super::mixer::{MIXER_NUM_CHANNELS, MIXER_SAMPLE_RATE};

/// Starting point for echo cancellation's estimate of the delay between the
/// speakers playing audio and the mic picking it up.
const STREAM_DELAY_MS: i32 = 50;

/// Microphone processing in WebRTC's own order: echo cancellation, then noise
/// suppression, then automatic gain.
///
/// The order matters for loudness: gain control caps its gain so the noise
/// floor stays low, so measuring un-suppressed noise holds speech down.
/// LiveKit's wrapper fixes the settings when an APM is created, so there is one
/// APM with noise suppression and one without. Both get the speaker signal to
/// keep echo cancellation warm, and the noise-cancellation setting picks one
/// per frame, so it can change mid-call.
pub struct CaptureProcessing {
    with_noise_suppression: AudioProcessingModule,
    without_noise_suppression: AudioProcessingModule,
    reverse_scratch: Vec<i16>,
}

impl Default for CaptureProcessing {
    fn default() -> Self {
        Self::new()
    }
}

impl CaptureProcessing {
    pub fn new() -> Self {
        let mut with_noise_suppression = AudioProcessingModule::new(true, true, false, true);
        let mut without_noise_suppression = AudioProcessingModule::new(true, true, false, false);
        for apm in [&mut with_noise_suppression, &mut without_noise_suppression] {
            if let Err(e) = apm.set_stream_delay_ms(STREAM_DELAY_MS) {
                log::warn!("CaptureProcessing: failed to set stream delay: {e:?}");
            }
        }
        Self {
            with_noise_suppression,
            without_noise_suppression,
            reverse_scratch: Vec::new(),
        }
    }

    /// Feeds one 10 ms mono frame of what the speakers play, for echo cancellation.
    pub fn process_reverse(&mut self, played: &[i16]) {
        for apm in [
            &mut self.with_noise_suppression,
            &mut self.without_noise_suppression,
        ] {
            self.reverse_scratch.clear();
            self.reverse_scratch.extend_from_slice(played);
            let _ = apm.process_reverse_stream(
                &mut self.reverse_scratch,
                MIXER_SAMPLE_RATE as i32,
                MIXER_NUM_CHANNELS as i32,
            );
        }
    }

    /// Processes one 10 ms mono mic frame at `MIXER_SAMPLE_RATE` in place.
    pub fn process_capture(&mut self, frame: &mut [i16], suppress_noise: bool) {
        let apm = if suppress_noise {
            &mut self.with_noise_suppression
        } else {
            &mut self.without_noise_suppression
        };
        let _ = apm.process_stream(frame, MIXER_SAMPLE_RATE as i32, MIXER_NUM_CHANNELS as i32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_LEN: usize = (MIXER_SAMPLE_RATE / 100) as usize;

    /// Quiet voice-like audio (about -35 dBFS while voiced) in steady background
    /// noise (about -48 dBFS), as from a headset mic in a normal room. Returns
    /// the frames and which ones are voiced. The voice is a harmonic series
    /// gliding 120-220 Hz, voiced 300 ms out of every 500 ms.
    fn quiet_voice_in_noise(frame_count: usize) -> (Vec<Vec<i16>>, Vec<bool>) {
        use std::f64::consts::PI;
        let rate = MIXER_SAMPLE_RATE as f64;
        let voice_amp = 10f64.powf(-35.0 / 20.0) * i16::MAX as f64 * 2f64.sqrt() / 1.3;
        let noise_amp = 10f64.powf(-48.0 / 20.0) * i16::MAX as f64 * 3f64.sqrt();
        let mut phase = 0f64;
        let mut state: u32 = 0x1234_5678;
        let mut frames = Vec::with_capacity(frame_count);
        let mut voiced = Vec::with_capacity(frame_count);
        for frame_index in 0..frame_count {
            let is_voiced = frame_index % 50 < 30;
            voiced.push(is_voiced);
            frames.push(
                (0..FRAME_LEN)
                    .map(|i| {
                        let t = (frame_index * FRAME_LEN + i) as f64 / rate;
                        let pitch = 170.0 + 50.0 * (2.0 * PI * 3.0 * t).sin();
                        phase += 2.0 * PI * pitch / rate;
                        let voice = if is_voiced {
                            (1..=10)
                                .map(|h| (phase * h as f64).sin() / h as f64)
                                .sum::<f64>()
                        } else {
                            0.0
                        };
                        state ^= state << 13;
                        state ^= state >> 17;
                        state ^= state << 5;
                        let noise = state as f64 / u32::MAX as f64 * 2.0 - 1.0;
                        (voice * voice_amp + noise * noise_amp) as i16
                    })
                    .collect(),
            );
        }
        (frames, voiced)
    }

    fn level_dbfs<'a>(frames: impl Iterator<Item = &'a Vec<i16>>) -> f64 {
        let (sum, count) = frames.flatten().fold((0f64, 0usize), |(sum, count), &s| {
            (sum + (s as f64).powi(2), count + 1)
        });
        10.0 * (sum / count.max(1) as f64 / (i16::MAX as f64).powi(2))
            .max(1e-12)
            .log10()
    }

    /// Speech and pause levels over the second half, once gain control has settled.
    fn speech_and_pause_dbfs(frames: &[Vec<i16>], voiced: &[bool]) -> (f64, f64) {
        let half = frames.len() / 2;
        let pick = |want_voiced: bool| {
            frames[half..]
                .iter()
                .zip(&voiced[half..])
                .filter(move |(_, &is_voiced)| is_voiced == want_voiced)
                .map(|(frame, _)| frame)
        };
        (level_dbfs(pick(true)), level_dbfs(pick(false)))
    }

    #[test]
    fn noise_suppression_lets_gain_control_lift_quiet_speech() {
        let (input, voiced) = quiet_voice_in_noise(1000);
        let mut processing = CaptureProcessing::new();
        let mut suppressed = input.clone();
        let mut plain = input;
        for (suppressed, plain) in suppressed.iter_mut().zip(plain.iter_mut()) {
            processing.process_capture(suppressed, true);
            processing.process_capture(plain, false);
        }

        let (speech_on, pause_on) = speech_and_pause_dbfs(&suppressed, &voiced);
        let (speech_off, pause_off) = speech_and_pause_dbfs(&plain, &voiced);
        // Measured: speech -22.8 vs -33.7 dBFS; speech-to-pause 11.6 vs 12.1 dB.
        assert!(
            speech_on >= speech_off + 6.0,
            "expected noise suppression to let speech be lifted by at least 6 dB: \
             {speech_on:.1} vs {speech_off:.1} dBFS"
        );
        assert!(
            speech_on - pause_on >= speech_off - pause_off - 2.0,
            "expected speech to stand out from the pauses about as much: \
             {:.1} vs {:.1} dB",
            speech_on - pause_on,
            speech_off - pause_off
        );
    }
}
