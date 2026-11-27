use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::StreamConfig;
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::native::audio_mixer::{self, AudioMixer};
use livekit::webrtc::native::audio_resampler::AudioResampler;
use log::{error, info};
use parking_lot::Mutex;
use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use super::processing::CaptureProcessing;

pub type SharedProcessor = Arc<Mutex<CaptureProcessing>>;

pub const MIXER_SAMPLE_RATE: u32 = 48000;
pub const MIXER_NUM_CHANNELS: u32 = 1;

/// Hard cap on per-source buffered frames (10ms each).
///
/// LiveKit's internal receive task runs on the main runtime and bursts frames
/// to us when it's CPU-starved. We absorb bursts with headroom; if we blow
/// past this cap we crash back by dropping from the front.
const HARD_CAP_FRAMES: usize = 80; // 800ms
/// Playback cushion per remote participant, in 10 ms frames. WebRTC's jitter
/// buffer absorbs network jitter; this covers stalls in local delivery, after
/// which the late frames arrive in a burst.
const TARGET_DELAY: usize = 6; // 60 ms
/// Frames to collect before playback starts, at the start of a call and after
/// the buffer ran dry. Playing the first frames as they trickle in leaves no
/// cushion, so the opening seconds of a call kept running dry.
const START_DELAY: usize = 5; // 50 ms
/// Above this the buffer drops speech too, to bound latency after a long stall.
const MAX_DELAY: usize = 15; // 150 ms
/// Frames quieter than this RMS (about -45 dBFS) can be skipped unheard.
const QUIET_RMS: f64 = 184.0;

/// Playback buffer counters for one remote participant. Updated from the
/// real-time output callback, so lock-free; read and reset by the receive task.
#[derive(Default)]
struct PlaybackStats {
    mixes: AtomicU32,
    depth_sum: AtomicU32,
    skipped_quiet: AtomicU32,
    dropped: AtomicU32,
    underruns: AtomicU32,
    playing: AtomicBool,
}

pub struct PlaybackSnapshot {
    /// Average frames buffered when the output callback asked for audio.
    pub avg_depth_frames: f64,
    /// Quiet frames skipped to shrink the buffer back to `TARGET_DELAY`.
    pub skipped_quiet: u32,
    /// Audible frames dropped because the buffer exceeded `MAX_DELAY`.
    pub dropped: u32,
    /// Times the buffer ran dry while audio was flowing (not while muted).
    pub underruns: u32,
}

fn rms(frame: &[i16]) -> f64 {
    if frame.is_empty() {
        return 0.0;
    }
    let sum: f64 = frame.iter().map(|&s| (s as f64).powi(2)).sum();
    (sum / frame.len() as f64).sqrt()
}

/// Takes the next frame to play. Above `TARGET_DELAY` the buffer shrinks only
/// by skipping quiet frames, so a burst after a stall becomes cushion and drains
/// during the next pause instead of cutting speech; above `MAX_DELAY` it drops
/// speech too. Playback (re)starts once `START_DELAY` frames are buffered.
fn next_frame(buffer: &mut VecDeque<Vec<i16>>, stats: &PlaybackStats) -> Option<Vec<i16>> {
    stats.mixes.fetch_add(1, Ordering::Relaxed);
    stats
        .depth_sum
        .fetch_add(buffer.len() as u32, Ordering::Relaxed);
    if !stats.playing.load(Ordering::Relaxed) && buffer.len() < START_DELAY {
        return None;
    }
    if buffer.len() > TARGET_DELAY {
        let front_is_quiet = buffer.front().is_some_and(|frame| rms(frame) < QUIET_RMS);
        if front_is_quiet {
            buffer.pop_front();
            stats.skipped_quiet.fetch_add(1, Ordering::Relaxed);
        } else if buffer.len() > MAX_DELAY {
            buffer.pop_front();
            stats.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
    match buffer.pop_front() {
        Some(frame) => {
            stats.playing.store(true, Ordering::Relaxed);
            Some(frame)
        }
        None => {
            if stats.playing.swap(false, Ordering::Relaxed) {
                stats.underruns.fetch_add(1, Ordering::Relaxed);
            }
            None
        }
    }
}

struct MixerInner {
    _stream: cpal::Stream,
    mixer: Arc<Mutex<AudioMixer>>,
    apm: SharedProcessor,
    next_ssrc: i32,
}

#[derive(Clone)]
pub struct MixerHandle {
    inner: Arc<Mutex<MixerInner>>,
}

impl std::fmt::Debug for MixerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MixerHandle").finish()
    }
}

#[derive(Clone)]
pub struct AudioSource {
    ssrc: i32,
    sample_rate: u32,
    num_channels: u32,
    last_mix: Arc<Mutex<Option<std::time::Instant>>>,
    buffer: Arc<Mutex<VecDeque<Vec<i16>>>>,
    stats: Arc<PlaybackStats>,
}

impl AudioSource {
    /// Returns the playback buffer counters since the previous call and resets them.
    pub fn take_stats(&self) -> PlaybackSnapshot {
        let mixes = self.stats.mixes.swap(0, Ordering::Relaxed);
        let depth_sum = self.stats.depth_sum.swap(0, Ordering::Relaxed);
        PlaybackSnapshot {
            avg_depth_frames: if mixes == 0 {
                0.0
            } else {
                depth_sum as f64 / mixes as f64
            },
            skipped_quiet: self.stats.skipped_quiet.swap(0, Ordering::Relaxed),
            dropped: self.stats.dropped.swap(0, Ordering::Relaxed),
            underruns: self.stats.underruns.swap(0, Ordering::Relaxed),
        }
    }

    pub fn push_samples(&self, samples: &[i16]) {
        let mut buffer = self.buffer.lock();
        buffer.push_back(samples.to_vec());
        if buffer.len() > HARD_CAP_FRAMES {
            while buffer.len() > HARD_CAP_FRAMES {
                buffer.pop_front();
            }
        }
    }
}

impl audio_mixer::AudioMixerSource for AudioSource {
    fn ssrc(&self) -> i32 {
        self.ssrc
    }

    fn preferred_sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn get_audio_frame_with_info(&self, _target_sample_rate: u32) -> Option<AudioFrame<'_>> {
        let now = std::time::Instant::now();
        let mut last = self.last_mix.lock();
        if let Some(prev) = *last {
            let gap = now.duration_since(prev);
            if gap.as_millis() > 100 {
                log::warn!(
                    "AudioSource: {}ms gap, flushing stale frames",
                    gap.as_millis()
                );
                self.buffer.lock().clear();
            }
        }
        *last = Some(now);
        drop(last);

        let buf = next_frame(&mut self.buffer.lock(), &self.stats)?;

        Some(AudioFrame {
            data: Cow::Owned(buf),
            sample_rate: self.sample_rate,
            num_channels: self.num_channels,
            samples_per_channel: self.sample_rate / 100,
        })
    }
}

/// Name of an output device, as cpal reports it.
fn output_device_name(device: &cpal::Device) -> Option<String> {
    #[allow(deprecated)]
    device.name().ok()
}

/// The speakers and headphones call audio can play on, with the system
/// default output flagged.
pub fn list_audio_outputs() -> Vec<socket_lib::AudioDevice> {
    let host = cpal::default_host();
    let default_name = host
        .default_output_device()
        .and_then(|device| output_device_name(&device));
    let devices = match host.output_devices() {
        Ok(devices) => devices,
        Err(e) => {
            error!("Failed to list audio outputs: {e}");
            return vec![];
        }
    };
    devices
        .filter_map(|device| output_device_name(&device))
        .map(|name| socket_lib::AudioDevice {
            default: default_name.as_deref() == Some(name.as_str()),
            name,
        })
        .collect()
}

/// Whether an output device with this name is connected.
pub fn output_device_exists(name: &str) -> bool {
    cpal::default_host()
        .output_devices()
        .is_ok_and(|mut devices| {
            devices.any(|device| output_device_name(&device).as_deref() == Some(name))
        })
}

/// The named output device, or the system default when it's `None` or no
/// longer connected.
fn find_output_device(name: Option<&str>) -> Option<cpal::Device> {
    let host = cpal::default_host();
    if let Some(name) = name {
        let found = host.output_devices().ok().and_then(|mut devices| {
            devices.find(|device| output_device_name(device).as_deref() == Some(name))
        });
        if found.is_some() {
            return found;
        }
        log::warn!("Audio output {name:?} is not connected, using the system default");
    }
    host.default_output_device()
}

fn open_output_stream(
    mixer: Arc<Mutex<AudioMixer>>,
    apm: SharedProcessor,
    output_device: Option<&str>,
) -> Result<cpal::Stream, String> {
    let device = find_output_device(output_device).ok_or("No audio output device")?;
    let cfg = device
        .default_output_config()
        .map_err(|e| format!("Failed to get output config: {e}"))?;

    let output_sample_rate = cfg.sample_rate();
    let output_channels = cfg.channels();
    let config = StreamConfig {
        channels: output_channels,
        sample_rate: output_sample_rate,
        buffer_size: cpal::BufferSize::Default,
    };

    info!(
        "cpal output: {:?} {}Hz {}ch",
        output_device_name(&device),
        output_sample_rate,
        output_channels
    );

    // Buffer draining pattern adapted from Zed's audio playback implementation:
    // https://github.com/zed-industries/zed/blob/main/crates/audio/src/audio.rs
    let mut resampler = AudioResampler::default();
    let mut buf: Vec<f32> = Vec::new();

    let stream = device
        .build_output_stream(
            &config,
            move |mut data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                while !data.is_empty() {
                    if data.len() <= buf.len() {
                        let rest = buf.split_off(data.len());
                        data.copy_from_slice(&buf);
                        buf = rest;
                        return;
                    }
                    if !buf.is_empty() {
                        let (prefix, suffix) = data.split_at_mut(buf.len());
                        prefix.copy_from_slice(&buf);
                        buf.clear();
                        data = suffix;
                    }
                    // Mix a new 10ms frame from all sources (mono at MIXER_SAMPLE_RATE)
                    let mut mixer_guard = mixer.lock();
                    let mixed = mixer_guard.mix(MIXER_NUM_CHANNELS as usize);
                    // Feed mono mix to APM reverse stream before resampling to
                    // multi-channel output.
                    apm.lock().process_reverse(mixed);
                    // WebRTC only upmixes mono to stereo. Keep that path for
                    // mono/stereo devices and handle wider outputs ourselves.
                    let resampler_channels = if output_channels > 2 {
                        MIXER_NUM_CHANNELS
                    } else {
                        output_channels as u32
                    };
                    let sampled = resampler.remix_and_resample(
                        mixed,
                        MIXER_SAMPLE_RATE / 100,
                        MIXER_NUM_CHANNELS,
                        MIXER_SAMPLE_RATE,
                        resampler_channels,
                        output_sample_rate,
                    );
                    if output_channels <= 2 {
                        buf = sampled
                            .iter()
                            .map(|&sample| sample as f32 / i16::MAX as f32)
                            .collect();
                    } else {
                        buf.clear();
                        buf.reserve(sampled.len() * output_channels as usize);
                        for &sample in sampled {
                            let normalized_sample = sample as f32 / i16::MAX as f32;
                            buf.extend((0..output_channels).map(|channel| {
                                if channel < 2 {
                                    normalized_sample
                                } else {
                                    0.0
                                }
                            }));
                        }
                        debug_assert_eq!(
                            buf.len(),
                            output_sample_rate as usize / 100 * output_channels as usize,
                        );
                    }
                }
            },
            |err| error!("cpal stream error: {err}"),
            None,
        )
        .map_err(|e| format!("Failed to build output stream: {e}"))?;

    stream
        .play()
        .map_err(|e| format!("Failed to start stream: {e}"))?;

    Ok(stream)
}

impl MixerHandle {
    /// Starts playback on `output_device`, or the system default output when `None`.
    pub fn new(output_device: Option<&str>) -> Result<(Self, SharedProcessor), String> {
        let apm = Arc::new(Mutex::new(CaptureProcessing::new()));
        let mixer = Arc::new(Mutex::new(AudioMixer::new()));
        let stream = open_output_stream(mixer.clone(), apm.clone(), output_device)?;
        let handle = Self {
            inner: Arc::new(Mutex::new(MixerInner {
                _stream: stream,
                mixer,
                apm: apm.clone(),
                next_ssrc: 1,
            })),
        };
        Ok((handle, apm))
    }

    pub fn add_source(&self, sample_rate: u32, channels: u16) -> AudioSource {
        let mut inner = self.inner.lock();
        let ssrc = inner.next_ssrc;
        inner.next_ssrc += 1;
        let source = AudioSource {
            ssrc,
            sample_rate,
            num_channels: channels as u32,
            last_mix: Arc::new(Mutex::new(None)),
            buffer: Arc::new(Mutex::new(VecDeque::new())),
            stats: Arc::new(PlaybackStats::default()),
        };
        inner.mixer.lock().add_source(source.clone());
        source
    }

    /// Reopens playback on `output_device`, or the system default output when `None`.
    pub fn reconnect(&self, output_device: Option<&str>) -> Result<(), String> {
        let mut inner = self.inner.lock();
        let stream = open_output_stream(inner.mixer.clone(), inner.apm.clone(), output_device)?;
        inner._stream = stream;
        info!("Audio output reconnected");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_LEN: usize = (MIXER_SAMPLE_RATE / 100) as usize;

    fn speech() -> Vec<i16> {
        vec![3000; FRAME_LEN]
    }

    fn quiet() -> Vec<i16> {
        vec![10; FRAME_LEN]
    }

    #[test]
    fn keeps_speech_above_target_until_max_delay() {
        let stats = PlaybackStats::default();
        let mut buffer: VecDeque<_> = (0..MAX_DELAY).map(|_| speech()).collect();
        assert!(next_frame(&mut buffer, &stats).is_some());
        assert_eq!(buffer.len(), MAX_DELAY - 1);
        assert_eq!(stats.dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn drops_speech_beyond_max_delay() {
        let stats = PlaybackStats::default();
        let mut buffer: VecDeque<_> = (0..=MAX_DELAY).map(|_| speech()).collect();
        assert!(next_frame(&mut buffer, &stats).is_some());
        assert_eq!(buffer.len(), MAX_DELAY - 1);
        assert_eq!(stats.dropped.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn skips_quiet_frames_above_target() {
        let stats = PlaybackStats::default();
        let mut buffer: VecDeque<_> = std::iter::once(quiet())
            .chain((0..TARGET_DELAY).map(|_| speech()))
            .collect();
        assert_eq!(next_frame(&mut buffer, &stats), Some(speech()));
        assert_eq!(stats.skipped_quiet.load(Ordering::Relaxed), 1);
        assert_eq!(stats.dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn plays_quiet_frames_at_or_below_target() {
        let stats = PlaybackStats::default();
        let mut buffer: VecDeque<_> = (0..TARGET_DELAY).map(|_| quiet()).collect();
        assert_eq!(next_frame(&mut buffer, &stats), Some(quiet()));
        assert_eq!(stats.skipped_quiet.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn counts_underrun_only_after_audio_flowed() {
        let stats = PlaybackStats::default();
        let mut buffer = VecDeque::new();
        assert!(next_frame(&mut buffer, &stats).is_none());
        assert_eq!(stats.underruns.load(Ordering::Relaxed), 0);

        buffer.extend((0..START_DELAY).map(|_| speech()));
        for _ in 0..START_DELAY {
            assert!(next_frame(&mut buffer, &stats).is_some());
        }
        assert!(next_frame(&mut buffer, &stats).is_none());
        assert!(next_frame(&mut buffer, &stats).is_none());
        assert_eq!(stats.underruns.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn waits_for_a_cushion_before_playing() {
        let stats = PlaybackStats::default();
        let mut buffer = VecDeque::new();
        for _ in 1..START_DELAY {
            buffer.push_back(speech());
            assert!(next_frame(&mut buffer, &stats).is_none());
        }
        buffer.push_back(speech());
        assert!(next_frame(&mut buffer, &stats).is_some());
        assert_eq!(buffer.len(), START_DELAY - 1);

        // Once playing, the cushion is played out without waiting again.
        buffer.truncate(1);
        assert!(next_frame(&mut buffer, &stats).is_some());

        // After running dry, playback waits for a full cushion again.
        assert!(next_frame(&mut buffer, &stats).is_none());
        buffer.push_back(speech());
        assert!(next_frame(&mut buffer, &stats).is_none());
        assert_eq!(stats.underruns.load(Ordering::Relaxed), 1);
    }
}
