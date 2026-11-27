use livekit::options::{AudioEncoding, TrackPublishOptions};
use livekit::track::{LocalAudioTrack, LocalTrack, RemoteAudioTrack, TrackSource};
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::prelude::{AudioSourceOptions, RtcAudioSource};
use livekit::Room;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::runtime::Handle as TokioHandle;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

use crate::audio::mixer::{AudioSource, MixerHandle, SharedProcessor, MIXER_SAMPLE_RATE};

pub const LIVEKIT_SAMPLE_RATE: u32 = MIXER_SAMPLE_RATE;
const AUDIO_MAX_BITRATE: u64 = 48_000;
pub const AUDIO_NUM_CHANNELS: u32 = 1;
const AUDIO_TRACK_NAME: &str = "microphone";
// 0 = LiveKit's direct path: each 10 ms frame goes straight to WebRTC instead of
// through a queue that, once full, stays full and adds its whole length as delay.
const AUDIO_QUEUE_SIZE: u32 = 0;
const PLAYBACK_STATS_INTERVAL: Duration = Duration::from_secs(10);
const SENT_LEVEL_INTERVAL: Duration = Duration::from_secs(10);
/// Outgoing frames louder than this count as speech in the level log.
const SPEECH_THRESHOLD_DBFS: f64 = -50.0;

pub struct AudioPublisher {
    audio_track: LocalAudioTrack,
    processing_task: tokio::task::JoinHandle<()>,
}

impl AudioPublisher {
    pub(crate) async fn publish(
        room: &Room,
        sample_rate: u32,
        sample_rx: mpsc::UnboundedReceiver<Vec<i16>>,
        processor: SharedProcessor,
        audio_handle: &TokioHandle,
        noise_suppression: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        let audio_source_options = AudioSourceOptions {
            echo_cancellation: false,
            noise_suppression: false,
            auto_gain_control: false,
        };
        let native_source = NativeAudioSource::new(
            audio_source_options,
            sample_rate,
            AUDIO_NUM_CHANNELS,
            AUDIO_QUEUE_SIZE,
        );

        let track = LocalAudioTrack::create_audio_track(
            AUDIO_TRACK_NAME,
            RtcAudioSource::Native(native_source.clone()),
        );

        let now = std::time::Instant::now();
        room.local_participant()
            .publish_track(
                LocalTrack::Audio(track.clone()),
                TrackPublishOptions {
                    source: TrackSource::Microphone,
                    audio_encoding: Some(AudioEncoding {
                        max_bitrate: AUDIO_MAX_BITRATE,
                    }),
                    dtx: false,
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| format!("Failed to publish audio track: {e}"))?;
        log::info!("audio_publish: took {}", now.elapsed().as_millis());

        let processing_task = audio_handle.spawn(process_audio_samples(
            sample_rx,
            native_source,
            sample_rate,
            processor,
            noise_suppression,
        ));

        log::info!("AudioPublisher: audio track published ({}Hz)", sample_rate,);

        Ok(Self {
            audio_track: track,
            processing_task,
        })
    }

    pub async fn unpublish(self, room: &Room) {
        self.processing_task.abort();
        let sid = self.audio_track.sid();
        let res = room.local_participant().unpublish_track(&sid).await;
        if let Err(e) = res {
            log::error!("AudioPublisher::unpublish: failed to unpublish track: {e:?}");
        }
        log::info!("AudioPublisher: audio track unpublished");
    }

    pub fn mute(&self) {
        self.audio_track.mute();
    }

    pub fn unmute(&self) {
        self.audio_track.unmute();
    }
}

async fn process_audio_samples(
    mut rx: mpsc::UnboundedReceiver<Vec<i16>>,
    audio_source: NativeAudioSource,
    sample_rate: u32,
    processor: SharedProcessor,
    noise_suppression: Arc<AtomicBool>,
) {
    assert_eq!(
        sample_rate, MIXER_SAMPLE_RATE,
        "Mic capture sample rate must match APM rate"
    );
    let samples_per_unit = (MIXER_SAMPLE_RATE / 100) as usize;
    let max_buffer_frames = 20;
    let max_buffer_samples = max_buffer_frames * samples_per_unit;

    log::info!(
        "Starting audio processing ({}Hz, {} samples per 10ms, max buffer {}ms)",
        sample_rate,
        samples_per_unit,
        max_buffer_frames * 10,
    );

    // Capture starts before the room connects. Sending that backlog would delay
    // everything said for the rest of the call, since it never drains.
    let mut discarded_samples = 0;
    while let Ok(stale) = rx.try_recv() {
        discarded_samples += stale.len();
    }
    log::info!(
        "Audio processing: discarded {}ms captured before the call connected",
        discarded_samples * 1000 / sample_rate as usize
    );

    let mut buffer: Vec<i16> = Vec::new();
    let mut chunk = vec![0i16; samples_per_unit];
    let mut sent_level = SentLevel::new();

    while let Some(audio_data) = rx.recv().await {
        // Drain all pending messages into buffer
        buffer.extend_from_slice(&audio_data);
        while let Ok(more) = rx.try_recv() {
            buffer.extend_from_slice(&more);
        }

        // Trim oldest frames if over budget, aligned to frame boundary
        if buffer.len() > max_buffer_samples {
            let total_frames = buffer.len() / samples_per_unit;
            let drop_frames = total_frames - max_buffer_frames;
            let drop_samples = drop_frames * samples_per_unit;
            log::warn!(
                "Audio capture: dropping {}ms ({} frames) to cap latency",
                drop_frames * 10,
                drop_frames,
            );
            buffer.drain(..drop_samples);
        }

        // Process all complete frames
        while buffer.len() >= samples_per_unit {
            chunk.copy_from_slice(&buffer[..samples_per_unit]);
            buffer.drain(..samples_per_unit);
            processor
                .lock()
                .process_capture(&mut chunk, noise_suppression.load(Ordering::Relaxed));
            sent_level.record(&chunk);
            capture_frame(&audio_source, &chunk, samples_per_unit, sample_rate).await;
        }
    }

    log::info!("Audio processing stopped");
}

/// Tracks how loud the outgoing mic audio is after processing, to compare
/// against other apps (and check that gain control does its job).
struct SentLevel {
    frames: u32,
    speech_frames: u32,
    speech_dbfs_sum: f64,
    peak_dbfs: f64,
    last_log: Instant,
}

impl SentLevel {
    fn new() -> Self {
        Self {
            frames: 0,
            speech_frames: 0,
            speech_dbfs_sum: 0.0,
            peak_dbfs: f64::NEG_INFINITY,
            last_log: Instant::now(),
        }
    }

    fn record(&mut self, frame: &[i16]) {
        let mean_square =
            frame.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / frame.len().max(1) as f64;
        let dbfs = 10.0 * (mean_square / (i16::MAX as f64).powi(2)).max(1e-12).log10();
        self.frames += 1;
        if dbfs > SPEECH_THRESHOLD_DBFS {
            self.speech_frames += 1;
            self.speech_dbfs_sum += dbfs;
            self.peak_dbfs = self.peak_dbfs.max(dbfs);
        }
        if self.last_log.elapsed() >= SENT_LEVEL_INTERVAL {
            if self.speech_frames > 0 {
                log::info!(
                    "Mic sent: speech {:.1} dBFS avg, {:.1} dBFS peak, active {}%",
                    self.speech_dbfs_sum / self.speech_frames as f64,
                    self.peak_dbfs,
                    self.speech_frames * 100 / self.frames,
                );
            } else {
                log::info!(
                    "Mic sent: no speech in the last {}s",
                    SENT_LEVEL_INTERVAL.as_secs()
                );
            }
            *self = Self::new();
        }
    }
}

const SYSTEM_AUDIO_TRACK_NAME: &str = "screen_share_audio";
/// System audio is often music or video sound, so it gets more bits than speech.
const SYSTEM_AUDIO_MAX_BITRATE: u64 = 96_000;
/// Samples per 10 ms frame at the LiveKit rate.
const FRAME_SAMPLES: usize = (LIVEKIT_SAMPLE_RATE / 100) as usize;
/// System audio waiting to be sent beyond this (200 ms) is dropped, to bound delay.
const SYSTEM_AUDIO_MAX_BUFFERED: usize = FRAME_SAMPLES * 20;

/// The sharer's system audio track: published muted with the screen share,
/// unmuted while system audio is shared. See `crate::audio::system_audio`.
pub struct SystemAudioPublisher {
    track: LocalAudioTrack,
    samples: mpsc::UnboundedSender<Vec<i16>>,
    task: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for SystemAudioPublisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemAudioPublisher")
            .finish_non_exhaustive()
    }
}

impl SystemAudioPublisher {
    pub(crate) async fn publish(room: &Room) -> Result<Self, String> {
        let source = NativeAudioSource::new(
            AudioSourceOptions {
                echo_cancellation: false,
                noise_suppression: false,
                auto_gain_control: false,
            },
            LIVEKIT_SAMPLE_RATE,
            AUDIO_NUM_CHANNELS,
            AUDIO_QUEUE_SIZE,
        );
        let track = LocalAudioTrack::create_audio_track(
            SYSTEM_AUDIO_TRACK_NAME,
            RtcAudioSource::Native(source.clone()),
        );
        track.mute();
        room.local_participant()
            .publish_track(
                LocalTrack::Audio(track.clone()),
                TrackPublishOptions {
                    source: TrackSource::ScreenshareAudio,
                    audio_encoding: Some(AudioEncoding {
                        max_bitrate: SYSTEM_AUDIO_MAX_BITRATE,
                    }),
                    dtx: true,
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| format!("Failed to publish the system audio track: {e}"))?;

        let (samples, mut samples_rx) = mpsc::unbounded_channel::<Vec<i16>>();
        let task = tokio::spawn(async move {
            let mut pending: Vec<i16> = Vec::new();
            while let Some(samples) = samples_rx.recv().await {
                pending.extend_from_slice(&samples);
                if pending.len() > SYSTEM_AUDIO_MAX_BUFFERED {
                    let excess = pending.len() - SYSTEM_AUDIO_MAX_BUFFERED;
                    pending.drain(..excess);
                }
                let whole_frames = pending.len() / FRAME_SAMPLES * FRAME_SAMPLES;
                for frame in pending[..whole_frames].chunks_exact(FRAME_SAMPLES) {
                    capture_frame(&source, frame, FRAME_SAMPLES, LIVEKIT_SAMPLE_RATE).await;
                }
                pending.drain(..whole_frames);
            }
        });
        log::info!("SystemAudioPublisher: system audio track published (muted)");
        Ok(Self {
            track,
            samples,
            task,
        })
    }

    /// Where screen capture sends system audio (48 kHz mono).
    pub fn sender(&self) -> mpsc::UnboundedSender<Vec<i16>> {
        self.samples.clone()
    }

    pub fn set_unmuted(&self, unmuted: bool) {
        if unmuted {
            self.track.unmute();
        } else {
            self.track.mute();
        }
    }

    pub async fn unpublish(self, room: &Room) {
        self.task.abort();
        if let Err(e) = room
            .local_participant()
            .unpublish_track(&self.track.sid())
            .await
        {
            log::error!("SystemAudioPublisher::unpublish: {e:?}");
        }
    }
}

/// Handle for a remote audio track subscription.
/// On drop, removes the source from the mixer and aborts the receive task.
pub struct AudioTrackHandle {
    _source: AudioSource,
    task: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for AudioTrackHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioTrackHandle").finish_non_exhaustive()
    }
}

impl Drop for AudioTrackHandle {
    fn drop(&mut self) {
        self.task.abort();
        log::info!("AudioTrackHandle dropped");
    }
}

/// Sets up a remote audio track to feed into the rodio mixer.
/// Returns a handle that cleans up automatically on drop.
pub fn play_remote_audio_track(
    track: RemoteAudioTrack,
    mixer: MixerHandle,
    participant_id: &str,
    audio_handle: &TokioHandle,
) -> AudioTrackHandle {
    let source = mixer.add_source(LIVEKIT_SAMPLE_RATE, AUDIO_NUM_CHANNELS as u16);
    let source_clone = source.clone();

    let mut stream = livekit::webrtc::audio_stream::native::NativeAudioStream::new(
        track.rtc_track(),
        LIVEKIT_SAMPLE_RATE as i32,
        AUDIO_NUM_CHANNELS as i32,
    );

    let stream_key = participant_id.to_string();
    let audio_handle = audio_handle.clone();
    let task = audio_handle.spawn(async move {
        log::info!("Starting audio receive loop for {}", stream_key);
        let mut frame_count: u64 = 0;
        let mut total_samples: u64 = 0;
        let start = std::time::Instant::now();
        let mut last_log = start;
        let mut last_stats_log = start;

        while let Some(frame) = stream.next().await {
            source_clone.push_samples(&frame.data);
            frame_count += 1;
            total_samples += frame.data.len() as u64;

            let now = std::time::Instant::now();
            if now.duration_since(last_stats_log) >= PLAYBACK_STATS_INTERVAL {
                let stats = source_clone.take_stats();
                log::info!(
                    "Playback buffer [{}]: avg {:.1} frames (~{:.0}ms), skipped quiet {}, dropped {}, underruns {}",
                    stream_key,
                    stats.avg_depth_frames,
                    stats.avg_depth_frames * 10.0,
                    stats.skipped_quiet,
                    stats.dropped,
                    stats.underruns,
                );
                last_stats_log = now;
            }
            if now.duration_since(last_log).as_secs() >= 5 {
                let elapsed = now.duration_since(start).as_secs_f64();
                let expected_secs = total_samples as f64 / LIVEKIT_SAMPLE_RATE as f64;
                let drift_ms = (expected_secs - elapsed) * 1000.0;
                if drift_ms.abs() > 50.0 {
                    log::debug!(
                        "Audio receive [{}]: drift {:.0}ms ({} frames, expected {:.1}s, wall {:.1}s)",
                        stream_key,
                        drift_ms,
                        frame_count,
                        expected_secs,
                        elapsed,
                    );
                }
                last_log = now;
            }
        }
        log::info!("Audio receive loop ended for {}", stream_key);
    });

    AudioTrackHandle {
        _source: source,
        task,
    }
}

async fn capture_frame(
    audio_source: &NativeAudioSource,
    data: &[i16],
    samples_per_channel: usize,
    sample_rate: u32,
) {
    let audio_frame = AudioFrame {
        data: data.into(),
        sample_rate,
        num_channels: AUDIO_NUM_CHANNELS,
        samples_per_channel: samples_per_channel as u32,
    };
    if let Err(e) = audio_source.capture_frame(&audio_frame).await {
        log::error!("Failed to send audio frame to LiveKit: {e}");
    }
}
