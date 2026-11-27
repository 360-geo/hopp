//! Transcribes calls on this Mac with Whisper (Settings > Transcription).
//!
//! The call's audio paths hand their samples to `feed_local` (what the
//! microphone sends) and `feed_remote` (what each teammate sends). A worker
//! thread splits each speaker's audio into utterances, transcribes them, and
//! writes the call's transcript as Markdown in the transcripts folder, line
//! by line as the call goes. At the end the file is rewritten in time order.

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Instant;

use chrono::{DateTime, Local};
use parking_lot::RwLock;

use super::segmenter::{Segmenter, Utterance, SAMPLE_RATE};

/// What a call's transcription needs, from Hopp's settings.
#[derive(Debug, Clone)]
pub struct TranscriptionConfig {
    pub model_path: PathBuf,
    pub transcripts_dir: PathBuf,
    /// Whisper language code, or "auto" to detect it per utterance.
    pub language: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Speaker {
    Local,
    Remote(String),
}

enum Input {
    Audio {
        speaker: Speaker,
        samples: Vec<i16>,
        at: Instant,
    },
    Finish,
}

/// The running transcription, if any. Audio paths check it every 10 ms, so
/// it's a cheap read lock.
static ACTIVE: RwLock<Option<mpsc::Sender<Input>>> = RwLock::new(None);

fn feed(speaker: Speaker, samples: &[i16]) {
    if let Some(sender) = ACTIVE.read().as_ref() {
        let _ = sender.send(Input::Audio {
            speaker,
            samples: samples.to_vec(),
            at: Instant::now(),
        });
    }
}

/// What the local microphone sends (48 kHz mono), while it isn't muted.
pub fn feed_local(samples: &[i16]) {
    feed(Speaker::Local, samples);
}

/// What a teammate sends (48 kHz mono).
pub fn feed_remote(name: &str, samples: &[i16]) {
    feed(Speaker::Remote(name.to_string()), samples);
}

/// Starts transcribing a call, if the model is there. `local_name` is the
/// user's name in the transcript.
pub fn start(config: TranscriptionConfig, local_name: String) {
    if !config.model_path.is_file() {
        log::warn!(
            "transcription: model not found at {}, not transcribing",
            config.model_path.display()
        );
        return;
    }
    let (sender, receiver) = mpsc::channel();
    {
        let mut active = ACTIVE.write();
        if active.is_some() {
            log::warn!("transcription: already running");
            return;
        }
        *active = Some(sender);
    }
    let spawned = std::thread::Builder::new()
        .name("transcription".to_string())
        .spawn(move || Worker::new(config, local_name).run(receiver));
    if let Err(e) = spawned {
        log::error!("transcription: failed to start the worker: {e}");
        ACTIVE.write().take();
    }
}

/// Stops transcribing: what's still being said is transcribed, then the
/// transcript is completed in the background.
pub fn stop() {
    if let Some(sender) = ACTIVE.write().take() {
        let _ = sender.send(Input::Finish);
    }
}

/// One line of the transcript.
#[derive(Debug, Clone, PartialEq)]
struct Line {
    at: DateTime<Local>,
    speaker: String,
    text: String,
}

impl Line {
    fn markdown(&self) -> String {
        format!(
            "**{} {}:** {}\n",
            self.at.format("%H:%M:%S"),
            self.speaker,
            self.text
        )
    }
}

struct Worker {
    config: TranscriptionConfig,
    local_name: String,
    started: Instant,
    started_wall: DateTime<Local>,
    segmenters: HashMap<Speaker, Segmenter>,
    lines: Vec<Line>,
    path: PathBuf,
    file: Option<File>,
}

impl Worker {
    fn new(config: TranscriptionConfig, local_name: String) -> Self {
        let started_wall = Local::now();
        let path = config.transcripts_dir.join(format!(
            "{} Hopp call.md",
            started_wall.format("%Y-%m-%d %H.%M")
        ));
        Self {
            config,
            local_name,
            started: Instant::now(),
            started_wall,
            segmenters: HashMap::new(),
            lines: Vec::new(),
            path,
            file: None,
        }
    }

    fn speaker_name(&self, speaker: &Speaker) -> String {
        match speaker {
            Speaker::Local => self.local_name.clone(),
            Speaker::Remote(name) if name.is_empty() => "Teammate".to_string(),
            Speaker::Remote(name) => name.clone(),
        }
    }

    fn run(mut self, receiver: mpsc::Receiver<Input>) {
        log::info!("transcription: started, writing {}", self.path.display());
        let mut engine = match whisper::Engine::load(&self.config.model_path, &self.config.language)
        {
            Ok(engine) => engine,
            Err(e) => {
                log::error!("transcription: failed to load the model: {e}");
                // Keep draining so feeders never block, until the call ends.
                while let Ok(input) = receiver.recv() {
                    if matches!(input, Input::Finish) {
                        break;
                    }
                }
                return;
            }
        };

        while let Ok(input) = receiver.recv() {
            match input {
                Input::Audio {
                    speaker,
                    samples,
                    at,
                } => {
                    let since_start = at.saturating_duration_since(self.started);
                    let done = self
                        .segmenters
                        .entry(speaker.clone())
                        .or_default()
                        .push(&samples, since_start);
                    for utterance in done {
                        self.transcribe(&mut engine, &speaker, utterance);
                    }
                }
                Input::Finish => break,
            }
        }

        let speakers: Vec<Speaker> = self.segmenters.keys().cloned().collect();
        for speaker in speakers {
            let rest = self.segmenters.get_mut(&speaker).and_then(Segmenter::flush);
            if let Some(utterance) = rest {
                self.transcribe(&mut engine, &speaker, utterance);
            }
        }
        self.finish();
    }

    fn transcribe(
        &mut self,
        engine: &mut whisper::Engine,
        speaker: &Speaker,
        utterance: Utterance,
    ) {
        let audio = to_whisper_input(&utterance.samples);
        let text = match engine.transcribe(&audio) {
            Ok(text) => text,
            Err(e) => {
                log::error!("transcription: Whisper failed: {e}");
                return;
            }
        };
        if text.is_empty() {
            return;
        }
        let at = self.started_wall
            + chrono::Duration::from_std(utterance.started_at).unwrap_or_default();
        let line = Line {
            at,
            speaker: self.speaker_name(speaker),
            text,
        };
        self.append(&line);
        self.lines.push(line);
    }

    /// Adds a line to the transcript file as the call goes, so it survives a crash.
    fn append(&mut self, line: &Line) {
        if self.file.is_none() {
            if let Err(e) = fs::create_dir_all(&self.config.transcripts_dir) {
                log::error!("transcription: failed to create the transcripts folder: {e}");
                return;
            }
            match OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
            {
                Ok(mut file) => {
                    let _ = write!(file, "{}", self.header(&[]));
                    self.file = Some(file);
                }
                Err(e) => {
                    log::error!("transcription: failed to open the transcript: {e}");
                    return;
                }
            }
        }
        if let Some(file) = self.file.as_mut() {
            if let Err(e) = file.write_all(line.markdown().as_bytes()) {
                log::error!("transcription: failed to write the transcript: {e}");
            }
        }
    }

    fn header(&self, speakers: &[String]) -> String {
        let mut header = format!(
            "# Hopp call, {}\n\n",
            self.started_wall.format("%-d %B %Y, %H:%M")
        );
        if !speakers.is_empty() {
            header.push_str(&format!("Speakers: {}\n\n", speakers.join(", ")));
        }
        header
    }

    /// Rewrites the transcript in time order, with who spoke.
    fn finish(mut self) {
        self.file = None;
        if self.lines.is_empty() {
            log::info!("transcription: nothing was said, no transcript");
            return;
        }
        self.lines.sort_by_key(|line| line.at);
        let speakers: Vec<String> = self
            .lines
            .iter()
            .map(|line| line.speaker.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut contents = self.header(&speakers);
        for line in &self.lines {
            contents.push_str(&line.markdown());
        }
        match write_atomically(&self.path, &contents) {
            Ok(()) => log::info!(
                "transcription: saved {} lines to {}",
                self.lines.len(),
                self.path.display()
            ),
            Err(e) => log::error!("transcription: failed to save the transcript: {e}"),
        }
    }
}

fn write_atomically(path: &Path, contents: &str) -> std::io::Result<()> {
    let partial = path.with_extension("md.partial");
    fs::write(&partial, contents)?;
    fs::rename(&partial, path)
}

/// Whisper takes 16 kHz mono floats: average each 3 samples of the 48 kHz
/// audio, which also filters out what 16 kHz can't hold.
fn to_whisper_input(samples: &[i16]) -> Vec<f32> {
    const FACTOR: usize = SAMPLE_RATE / 16_000;
    samples
        .chunks_exact(FACTOR)
        .map(|chunk| chunk.iter().map(|&s| s as f32).sum::<f32>() / (FACTOR as f32 * 32_768.0))
        .collect()
}

/// Text Whisper is known to invent on silence or music, from the subtitles it
/// was trained on.
const HALLUCINATIONS: &[&str] = &[
    "ondertiteling",
    "ondertiteld door",
    "amara.org",
    "thanks for watching",
    "thank you for watching",
    "bedankt voor het kijken",
    "bedankt voor het luisteren",
    "subtitles by",
];

/// Cleans up one Whisper segment: drops non-speech markers and known inventions.
fn clean_segment(text: &str) -> Option<String> {
    let text = text.trim();
    let lowercase = text.to_lowercase();
    let is_marker = (text.starts_with('[') && text.ends_with(']'))
        || (text.starts_with('(') && text.ends_with(')'))
        || text.chars().all(|c| !c.is_alphanumeric());
    if text.is_empty() || is_marker || HALLUCINATIONS.iter().any(|h| lowercase.contains(h)) {
        return None;
    }
    Some(text.to_string())
}

#[cfg(target_os = "macos")]
mod whisper {
    use std::path::Path;
    use whisper_rs::{
        FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
    };

    /// Segments Whisper itself thinks are probably not speech are dropped.
    const NO_SPEECH_PROBABILITY_LIMIT: f32 = 0.6;

    pub struct Engine {
        // The state borrows nothing from the context at the type level, but
        // needs it alive: keep both.
        _context: WhisperContext,
        state: WhisperState,
        language: String,
    }

    impl Engine {
        pub fn load(model: &Path, language: &str) -> Result<Self, String> {
            whisper_rs::install_logging_hooks();
            let started = std::time::Instant::now();
            let path = model.to_str().ok_or("model path is not UTF-8")?;
            let context =
                WhisperContext::new_with_params(path, WhisperContextParameters::default())
                    .map_err(|e| e.to_string())?;
            let state = context.create_state().map_err(|e| e.to_string())?;
            log::info!(
                "transcription: model loaded in {} ms",
                started.elapsed().as_millis()
            );
            Ok(Self {
                _context: context,
                state,
                language: language.to_string(),
            })
        }

        pub fn transcribe(&mut self, audio: &[f32]) -> Result<String, String> {
            let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            params.set_language(Some(&self.language));
            params.set_translate(false);
            params.set_no_context(true);
            params.set_suppress_blank(true);
            params.set_suppress_nst(true);
            params.set_no_speech_thold(NO_SPEECH_PROBABILITY_LIMIT);
            params.set_print_special(false);
            params.set_print_progress(false);
            params.set_print_realtime(false);
            params.set_print_timestamps(false);
            params.set_n_threads(4);
            self.state.full(params, audio).map_err(|e| e.to_string())?;

            let mut text = String::new();
            for index in 0..self.state.full_n_segments() {
                let Some(segment) = self.state.get_segment(index) else {
                    continue;
                };
                if segment.no_speech_probability() > NO_SPEECH_PROBABILITY_LIMIT {
                    continue;
                }
                let Ok(raw) = segment.to_str_lossy() else {
                    continue;
                };
                if let Some(clean) = super::clean_segment(&raw) {
                    if !text.is_empty() {
                        text.push(' ');
                    }
                    text.push_str(&clean);
                }
            }
            Ok(text)
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod whisper {
    use std::path::Path;

    pub struct Engine;

    impl Engine {
        pub fn load(_model: &Path, _language: &str) -> Result<Self, String> {
            Err("Transcription is only supported on macOS".to_string())
        }

        pub fn transcribe(&mut self, _audio: &[f32]) -> Result<String, String> {
            Ok(String::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downsamples_to_16k_floats() {
        let input = [16_384i16, 16_384, 16_384, -32_768, -32_768, -32_768, 7];
        assert_eq!(to_whisper_input(&input), vec![0.5, -1.0]);
    }

    #[test]
    fn keeps_speech_and_drops_markers_and_inventions() {
        assert_eq!(
            clean_segment("  Hoi Jaco, zie je mijn scherm? "),
            Some("Hoi Jaco, zie je mijn scherm?".to_string())
        );
        for text in [
            "",
            "[Music]",
            "(muziek)",
            "...",
            "Ondertiteld door de Amara.org gemeenschap",
            "Thanks for watching!",
        ] {
            assert_eq!(clean_segment(text), None, "{text:?}");
        }
    }

    #[test]
    fn formats_lines() {
        use chrono::TimeZone;
        let at = Local.with_ymd_and_hms(2026, 11, 27, 9, 5, 7).unwrap();
        let line = Line {
            at,
            speaker: "George".to_string(),
            text: "Hallo".to_string(),
        };
        assert_eq!(line.markdown(), "**09:05:07 George:** Hallo\n");
    }

    #[test]
    fn writes_a_sorted_transcript_with_speakers() {
        let dir = std::env::temp_dir().join(format!("hopp-transcript-test-{}", std::process::id()));
        let config = TranscriptionConfig {
            model_path: PathBuf::new(),
            transcripts_dir: dir.clone(),
            language: "auto".to_string(),
        };
        let mut worker = Worker::new(config, "George".to_string());
        let base = worker.started_wall;
        let late = Line {
            at: base + chrono::Duration::seconds(5),
            speaker: "Jaco".to_string(),
            text: "Ja".to_string(),
        };
        let early = Line {
            at: base + chrono::Duration::seconds(2),
            speaker: "George".to_string(),
            text: "Zie je het?".to_string(),
        };
        // Completed out of order, as utterances of different speakers can.
        worker.append(&late);
        worker.lines.push(late);
        worker.append(&early);
        worker.lines.push(early);
        let path = worker.path.clone();
        worker.finish();

        let contents = fs::read_to_string(&path).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert!(contents.starts_with("# Hopp call, "));
        assert!(contents.contains("Speakers: George, Jaco\n"));
        let george = contents.find("George:** Zie je het?").unwrap();
        let jaco = contents.find("Jaco:** Ja").unwrap();
        assert!(george < jaco);
    }

    /// Needs a Whisper model and a 48 kHz mono 16-bit WAV of Dutch speech:
    /// HOPP_TEST_WHISPER_MODEL=... HOPP_TEST_SPEECH_WAV=... cargo test -- --ignored
    #[test]
    #[ignore]
    fn transcribes_speech_end_to_end() {
        let model = std::env::var("HOPP_TEST_WHISPER_MODEL").expect("HOPP_TEST_WHISPER_MODEL");
        let wav =
            fs::read(std::env::var("HOPP_TEST_SPEECH_WAV").expect("HOPP_TEST_SPEECH_WAV")).unwrap();
        let data = wav.windows(4).position(|w| w == b"data").unwrap() + 8;
        let samples: Vec<i16> = wav[data..]
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        let dir = std::env::temp_dir().join(format!("hopp-whisper-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        start(
            TranscriptionConfig {
                model_path: model.into(),
                transcripts_dir: dir.clone(),
                language: "nl".to_string(),
            },
            "George".to_string(),
        );
        let mut audio = vec![0i16; 48_000];
        audio.extend(samples);
        audio.extend(vec![0i16; 48_000]);
        for chunk in audio.chunks(480) {
            feed_local(chunk);
        }
        stop();

        let deadline = Instant::now() + std::time::Duration::from_secs(120);
        let transcript = loop {
            let done = fs::read_dir(&dir).ok().and_then(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .find(|path| path.extension().is_some_and(|ext| ext == "md"))
                    .and_then(|path| fs::read_to_string(path).ok())
                    .filter(|contents| contents.contains("Speakers:"))
            });
            if let Some(done) = done {
                break done;
            }
            assert!(Instant::now() < deadline, "no transcript");
            std::thread::sleep(std::time::Duration::from_millis(200));
        };
        let _ = fs::remove_dir_all(&dir);
        println!("{transcript}");
        assert!(transcript.contains("George:**"));
        // Even the tiny model gets these words of the sample right.
        assert!(transcript.to_lowercase().contains("nieuwe versie"));
    }

    #[test]
    fn feeding_without_a_transcription_does_nothing() {
        feed_local(&[1, 2, 3]);
        feed_remote("Jaco", &[1, 2, 3]);
        stop();
    }
}
