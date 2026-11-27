//! Call transcription on this Mac (Settings > Transcription): where the
//! Whisper model and the transcripts live, downloading the model, and the
//! settings the core gets. The core does the transcribing.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use socket_lib::TranscriptionSettings;
use tauri::{AppHandle, Emitter};
use tauri_plugin_http::reqwest;

use crate::app_state::UserSettings;

/// Whisper large-v3 turbo, 5-bit quantized: about 550 MB, close to the full
/// model's accuracy, and fast on Apple Silicon GPUs. Tuple uses the same model.
const MODEL_FILE: &str = "ggml-large-v3-turbo-q5_0.bin";
const MODEL_URL: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin";
/// A download smaller than this is an error page, not the model.
const MIN_MODEL_BYTES: u64 = 100_000_000;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

static DOWNLOADING: AtomicBool = AtomicBool::new(false);

pub fn model_path(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("models").join(MODEL_FILE)
}

pub fn transcripts_dir(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("transcripts")
}

/// What the core needs to transcribe calls, or `None` when it shouldn't:
/// turned off, or the model isn't downloaded yet.
pub fn core_settings(
    app_data_dir: &Path,
    settings: &UserSettings,
) -> Option<TranscriptionSettings> {
    let model = model_path(app_data_dir);
    (settings.transcription_enabled && model.is_file()).then(|| TranscriptionSettings {
        model_path: model.to_string_lossy().into_owned(),
        transcripts_dir: transcripts_dir(app_data_dir).to_string_lossy().into_owned(),
        language: settings.transcription_language.clone(),
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct TranscriptionInfo {
    pub model_downloaded: bool,
    pub model_downloading: bool,
    pub transcripts_dir: String,
}

pub fn info(app_data_dir: &Path) -> TranscriptionInfo {
    TranscriptionInfo {
        model_downloaded: model_path(app_data_dir).is_file(),
        model_downloading: DOWNLOADING.load(Ordering::Relaxed),
        transcripts_dir: transcripts_dir(app_data_dir).to_string_lossy().into_owned(),
    }
}

#[derive(Debug, Clone, Serialize)]
struct Progress {
    downloaded: u64,
    total: Option<u64>,
}

/// Downloads the Whisper model, reporting `transcription_model_progress`
/// events. A partial file only becomes the model once it's complete.
pub async fn download_model(app: &AppHandle, app_data_dir: &Path) -> Result<(), String> {
    if DOWNLOADING.swap(true, Ordering::SeqCst) {
        return Err("The model is already downloading".to_string());
    }
    let result = download(app, &model_path(app_data_dir)).await;
    DOWNLOADING.store(false, Ordering::SeqCst);
    result
}

async fn download(app: &AppHandle, model: &Path) -> Result<(), String> {
    let dir = model.parent().ok_or("Invalid model path")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("Failed to create {}: {e}", dir.display()))?;
    let partial = model.with_extension("bin.partial");

    log::info!("transcription: downloading the model from {MODEL_URL}");
    let mut response = reqwest::get(MODEL_URL)
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| format!("Download failed: {e}"))?;
    let total = response.content_length();
    let mut file = std::fs::File::create(&partial)
        .map_err(|e| format!("Failed to create {}: {e}", partial.display()))?;
    let mut downloaded: u64 = 0;
    let mut last_progress = Instant::now();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("Download failed: {e}"))?
    {
        file.write_all(&chunk)
            .map_err(|e| format!("Failed to save the model: {e}"))?;
        downloaded += chunk.len() as u64;
        if last_progress.elapsed() >= PROGRESS_INTERVAL {
            last_progress = Instant::now();
            let _ = app.emit(
                "transcription_model_progress",
                Progress { downloaded, total },
            );
        }
    }
    file.flush()
        .map_err(|e| format!("Failed to save the model: {e}"))?;
    drop(file);

    if downloaded < MIN_MODEL_BYTES || total.is_some_and(|total| total != downloaded) {
        let _ = std::fs::remove_file(&partial);
        return Err(format!(
            "The download is incomplete ({downloaded} of {total:?} bytes)"
        ));
    }
    std::fs::rename(&partial, model).map_err(|e| format!("Failed to save the model: {e}"))?;
    let _ = app.emit(
        "transcription_model_progress",
        Progress {
            downloaded,
            total: Some(downloaded),
        },
    );
    log::info!("transcription: model downloaded ({downloaded} bytes)");
    Ok(())
}
