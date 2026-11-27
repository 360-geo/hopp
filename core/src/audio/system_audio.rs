//! Shares the Mac's sound with viewers while sharing the screen (Settings >
//! Screen Share), like Tuple's "share system audio when using headphones".
//!
//! Screen capture records the system audio (without Hopp's own call audio)
//! and sends it here. It goes to viewers only while headphones are in use:
//! through speakers the microphone would pick the sound up as well, and
//! viewers would hear it twice.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Where screen capture sends system audio (48 kHz mono), and whether it
/// should right now.
#[derive(Clone)]
pub struct SystemAudioTap {
    samples: mpsc::UnboundedSender<Vec<i16>>,
    enabled: Arc<AtomicBool>,
}

impl SystemAudioTap {
    pub fn new(samples: mpsc::UnboundedSender<Vec<i16>>, enabled: Arc<AtomicBool>) -> Self {
        Self { samples, enabled }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn send(&self, samples: Vec<i16>) {
        let _ = self.samples.send(samples);
    }
}

fn to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

/// Mixes float audio with one slice per channel (ScreenCaptureKit's
/// non-interleaved layout) down to mono 16-bit.
pub fn mono_from_planes(planes: &[Vec<f32>]) -> Vec<i16> {
    let Some(frames) = planes.iter().map(Vec::len).min() else {
        return vec![];
    };
    let channels = planes.len() as f32;
    (0..frames)
        .map(|i| to_i16(planes.iter().map(|plane| plane[i]).sum::<f32>() / channels))
        .collect()
}

/// Mixes interleaved float audio with `channels` channels down to mono 16-bit.
pub fn mono_from_interleaved(samples: &[f32], channels: usize) -> Vec<i16> {
    if channels == 0 {
        return vec![];
    }
    samples
        .chunks_exact(channels)
        .map(|frame| to_i16(frame.iter().sum::<f32>() / channels as f32))
        .collect()
}

/// Reads native-endian 32-bit floats from raw audio buffer bytes.
pub fn f32_samples(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn averages_channel_planes() {
        let left = vec![1.0, 0.5, -1.0];
        let right = vec![0.0, 0.5, -1.0];
        assert_eq!(
            mono_from_planes(&[left, right]),
            vec![i16::MAX / 2, i16::MAX / 2, -i16::MAX]
        );
    }

    #[test]
    fn averages_interleaved_frames_and_clips() {
        assert_eq!(
            mono_from_interleaved(&[2.0, 2.0, 0.0, 0.5, 0.25], 2),
            vec![i16::MAX, i16::MAX / 4]
        );
        assert!(mono_from_interleaved(&[0.5], 0).is_empty());
    }

    #[test]
    fn reads_float_bytes() {
        let bytes: Vec<u8> = [0.5f32, -0.25]
            .iter()
            .flat_map(|s| s.to_ne_bytes())
            .collect();
        assert_eq!(f32_samples(&bytes), vec![0.5, -0.25]);
    }
}
