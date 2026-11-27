//! Splits one speaker's audio into utterances at pauses, so each can be
//! transcribed on its own: Whisper takes up to 30 s at a time, and works best
//! on whole sentences without long silences.
//!
//! Speech is told from silence by loudness against the speaker's own
//! background noise, in 10 ms frames.

use std::collections::VecDeque;
use std::time::Duration;

/// Input sample rate: call audio is 48 kHz mono.
pub const SAMPLE_RATE: usize = 48_000;
const FRAME_SAMPLES: usize = SAMPLE_RATE / 100;

/// Frames must be at least this loud, and this far above the background noise, to count as speech.
const SPEECH_FLOOR_DBFS: f32 = -45.0;
const SPEECH_ABOVE_NOISE_DB: f32 = 9.0;
/// The noise estimate drops right away and rises at most this much per frame
/// (10 dB/s). The short gaps between words keep it low while someone talks.
const NOISE_RISE_DB_PER_FRAME: f32 = 0.1;
/// A pause this long (600 ms) ends an utterance.
const END_SILENCE_FRAMES: usize = 60;
/// Utterances with less speech than this (250 ms) are dropped: coughs, clicks.
const MIN_SPEECH_FRAMES: usize = 25;
/// Utterances are cut at 25 s, within Whisper's 30 s window.
const MAX_UTTERANCE_FRAMES: usize = 2_500;
/// Audio kept from just before speech starts (200 ms), so first syllables aren't clipped.
const PRE_ROLL_FRAMES: usize = 20;

/// A stretch of speech: when it started (since the call began) and its samples.
#[derive(Debug, Clone, PartialEq)]
pub struct Utterance {
    pub started_at: Duration,
    pub samples: Vec<i16>,
}

pub struct Segmenter {
    /// Samples waiting to fill a whole frame, and when the first of them arrived.
    partial: Vec<i16>,
    partial_at: Duration,
    noise_floor_dbfs: f32,
    pre_roll: VecDeque<(Duration, Vec<i16>)>,
    current: Option<Current>,
}

struct Current {
    started_at: Duration,
    samples: Vec<i16>,
    frames: usize,
    speech_frames: usize,
    silent_run: usize,
}

impl Default for Segmenter {
    fn default() -> Self {
        Self {
            partial: Vec::with_capacity(FRAME_SAMPLES),
            partial_at: Duration::ZERO,
            noise_floor_dbfs: -60.0,
            pre_roll: VecDeque::with_capacity(PRE_ROLL_FRAMES),
            current: None,
        }
    }
}

fn frame_dbfs(frame: &[i16]) -> f32 {
    let sum_sq: f64 = frame.iter().map(|&s| (s as f64) * (s as f64)).sum();
    let rms = (sum_sq / frame.len().max(1) as f64).sqrt() / i16::MAX as f64;
    (20.0 * rms.max(1e-9).log10()) as f32
}

impl Segmenter {
    /// Takes audio that arrived `at` (since the call began). Returns the
    /// utterances it completes.
    pub fn push(&mut self, samples: &[i16], at: Duration) -> Vec<Utterance> {
        let mut done = Vec::new();
        let mut offset = 0;
        while offset < samples.len() {
            if self.partial.is_empty() {
                let samples_before = offset as u64 * 1_000_000 / SAMPLE_RATE as u64;
                self.partial_at = at + Duration::from_micros(samples_before);
            }
            let take = (FRAME_SAMPLES - self.partial.len()).min(samples.len() - offset);
            self.partial
                .extend_from_slice(&samples[offset..offset + take]);
            offset += take;
            if self.partial.len() == FRAME_SAMPLES {
                let frame = std::mem::replace(&mut self.partial, Vec::with_capacity(FRAME_SAMPLES));
                if let Some(utterance) = self.push_frame(frame, self.partial_at) {
                    done.push(utterance);
                }
            }
        }
        done
    }

    /// Ends the utterance in progress, if it has enough speech (at call end).
    pub fn flush(&mut self) -> Option<Utterance> {
        self.current.take().and_then(Current::into_utterance)
    }

    fn push_frame(&mut self, frame: Vec<i16>, at: Duration) -> Option<Utterance> {
        let dbfs = frame_dbfs(&frame);
        let is_speech =
            dbfs >= SPEECH_FLOOR_DBFS.max(self.noise_floor_dbfs + SPEECH_ABOVE_NOISE_DB);
        self.noise_floor_dbfs = dbfs.min(self.noise_floor_dbfs + NOISE_RISE_DB_PER_FRAME);

        let Some(current) = self.current.as_mut() else {
            if is_speech {
                let started_at = self.pre_roll.front().map_or(at, |(time, _)| *time);
                let mut samples: Vec<i16> = self
                    .pre_roll
                    .drain(..)
                    .flat_map(|(_, frame)| frame)
                    .collect();
                samples.extend_from_slice(&frame);
                self.current = Some(Current {
                    started_at,
                    frames: samples.len() / FRAME_SAMPLES,
                    samples,
                    speech_frames: 1,
                    silent_run: 0,
                });
            } else {
                if self.pre_roll.len() == PRE_ROLL_FRAMES {
                    self.pre_roll.pop_front();
                }
                self.pre_roll.push_back((at, frame));
            }
            return None;
        };

        current.samples.extend_from_slice(&frame);
        current.frames += 1;
        if is_speech {
            current.speech_frames += 1;
            current.silent_run = 0;
        } else {
            current.silent_run += 1;
        }
        if current.silent_run >= END_SILENCE_FRAMES || current.frames >= MAX_UTTERANCE_FRAMES {
            return self.current.take().and_then(Current::into_utterance);
        }
        None
    }
}

impl Current {
    fn into_utterance(self) -> Option<Utterance> {
        (self.speech_frames >= MIN_SPEECH_FRAMES).then_some(Utterance {
            started_at: self.started_at,
            samples: self.samples,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_DURATION: Duration = Duration::from_millis(10);

    /// `frames` 10 ms frames of a tone at `dbfs` RMS.
    fn tone(dbfs: f32, frames: usize) -> Vec<i16> {
        let amplitude = 10f32.powf(dbfs / 20.0) * std::f32::consts::SQRT_2 * i16::MAX as f32;
        (0..frames * FRAME_SAMPLES)
            .map(|i| (amplitude * (i as f32 * 0.07).sin()) as i16)
            .collect()
    }

    /// `frames` 10 ms frames shaped like speech: 300 ms of voice, 50 ms gaps.
    fn speech(frames: usize) -> Vec<i16> {
        let voice = tone(-25.0, 1);
        let gap = tone(-60.0, 1);
        (0..frames)
            .flat_map(|i| {
                if i % 35 < 30 {
                    voice.clone()
                } else {
                    gap.clone()
                }
            })
            .collect()
    }

    /// Feeds audio in 10 ms chunks with matching arrival times, from `start` frames in.
    fn feed(segmenter: &mut Segmenter, audio: &[i16], start: usize) -> Vec<Utterance> {
        audio
            .chunks(FRAME_SAMPLES)
            .enumerate()
            .flat_map(|(i, chunk)| segmenter.push(chunk, FRAME_DURATION * (start + i) as u32))
            .collect()
    }

    #[test]
    fn splits_speech_at_pauses_with_pre_roll() {
        let mut segmenter = Segmenter::default();
        let mut audio = tone(-70.0, 100);
        audio.extend(speech(150));
        audio.extend(tone(-70.0, 100));
        let utterances = feed(&mut segmenter, &audio, 0);
        assert_eq!(utterances.len(), 1);
        // Speech starts at 1.0 s; the utterance keeps 200 ms from before it.
        assert_eq!(utterances[0].started_at, Duration::from_millis(800));
        let frames = utterances[0].samples.len() / FRAME_SAMPLES;
        assert_eq!(frames, PRE_ROLL_FRAMES + 150 + END_SILENCE_FRAMES);
    }

    #[test]
    fn drops_short_noises() {
        let mut segmenter = Segmenter::default();
        let mut audio = tone(-70.0, 50);
        audio.extend(tone(-20.0, 10));
        audio.extend(tone(-70.0, 100));
        assert!(feed(&mut segmenter, &audio, 0).is_empty());
    }

    #[test]
    fn ignores_steady_background_noise() {
        let mut segmenter = Segmenter::default();
        // A fan at -40 dBFS: loud enough, but the noise estimate catches up
        // within about a second.
        let audio = tone(-40.0, 3_000);
        let utterances = feed(&mut segmenter, &audio, 0);
        assert!(utterances.len() <= 1);
        assert!(
            utterances
                .iter()
                .all(|u| u.samples.len() / FRAME_SAMPLES < 300),
            "steady noise kept as a long utterance"
        );
    }

    #[test]
    fn cuts_long_speech_and_flushes_the_rest() {
        let mut segmenter = Segmenter::default();
        let audio = speech(MAX_UTTERANCE_FRAMES + 300);
        let utterances = feed(&mut segmenter, &audio, 0);
        assert_eq!(utterances.len(), 1);
        assert_eq!(
            utterances[0].samples.len() / FRAME_SAMPLES,
            MAX_UTTERANCE_FRAMES
        );
        let rest = segmenter.flush().expect("the rest of the speech");
        assert_eq!(rest.samples.len() / FRAME_SAMPLES, 300);
    }

    #[test]
    fn handles_chunks_that_split_frames() {
        let mut segmenter = Segmenter::default();
        let mut audio = tone(-70.0, 30);
        audio.extend(speech(100));
        audio.extend(tone(-70.0, 80));
        let utterances: Vec<_> = audio
            .chunks(1024)
            .enumerate()
            .flat_map(|(i, chunk)| {
                segmenter.push(
                    chunk,
                    Duration::from_micros(i as u64 * 1024 * 1_000_000 / 48_000),
                )
            })
            .collect();
        assert_eq!(utterances.len(), 1);
        // Chunk times are rounded to whole microseconds.
        let started = utterances[0].started_at.as_secs_f64();
        assert!((started - 0.1).abs() < 0.001, "started at {started}");
    }
}
