// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use hopp::sounds::{self, SoundConfig};
use log::LevelFilter;
use socket_lib::{
    AudioCaptureMessage, AudioDevice, CameraDevice, DrawingEnabled, Message, ScreenSharePickerMode,
    ScreenShareResolution, SentryMetadata,
};
use std::sync::mpsc as std_mpsc;
use tauri::Manager;
use tauri::{
    menu::{MenuBuilder, MenuItemBuilder},
    path::BaseDirectory,
    Emitter,
};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

use tauri_plugin_log::{Target, TargetKind};

use hopp::{
    app_state::{resolve_microphone, AppState, AppVeilApplication, UserSettings},
    application_catalog::InstalledApplication,
    create_core_process, get_log_level, get_log_path, get_sentry_dsn, permissions, ping_frontend,
    recv_expected_response, setup_start_on_launch, setup_tray_icon, AppData,
};
mod shortcuts;
#[cfg(target_os = "macos")]
use hopp::{
    disable_app_nap, set_window_corner_radius_and_decorations, yield_activation_to_core,
    CORNER_RADIUS,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::{env, sync::Arc};

#[cfg(any(target_os = "windows", target_os = "linux"))]
use tauri::PhysicalPosition;

#[tauri::command(async)]
async fn open_stats_window(app: tauri::AppHandle) {
    log::info!("open_stats_window");
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    if let Err(e) = data.sender.send(Message::OpenStatsWindow) {
        log::error!("open_stats_window: failed to send message: {e:?}");
    }
}

#[tauri::command(async)]
async fn stop_sharing(app: tauri::AppHandle) {
    log::info!("stop_sharing");
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    if let Err(e) = data.sender.send(Message::StopScreenshare) {
        log::error!("stop_sharing: failed to send message: {e:?}");
    }
}

#[tauri::command(async)]
async fn get_available_content(app: tauri::AppHandle) -> Result<(), String> {
    log::info!("get_available_content: open core screen selection");

    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();

    let remote_control_enabled = data.app_state.user_settings().remote_control_enabled;
    data.sender
        .send(Message::ControllerCursorEnabled(remote_control_enabled))
        .map_err(|e| {
            log::error!("get_available_content: failed to apply remote control setting: {e:?}");
            "Failed to apply remote control setting".to_string()
        })?;

    data.sender.send(Message::GetAvailableContent).map_err(|e| {
        log::error!("get_available_content: failed to send message: {e:?}");
        "Failed to start screen selection".to_string()
    })
}

#[tauri::command(async)]
fn play_sound(app: tauri::AppHandle, sound_name: String) {
    log::info!("play_sound");
    let tmp_sound_name = sound_name.split("/").last();
    if let Some(tmp_sound_name) = tmp_sound_name {
        log::info!("Playing sound: {}", tmp_sound_name);
    }
    /*
     * Check if the sound is already playing, if it has finished we
     * remove the entry from the sound_entries vector.
     */
    {
        let data = app.state::<Mutex<AppData>>();
        let mut data = data.lock().unwrap();
        let mut i = 0;
        while i < data.sound_entries.len() {
            if data.sound_entries[i].name == sound_name {
                /* Send a message to see if the sound is still playing */
                let res = data.sound_entries[i].tx.send(sounds::SoundCommand::Ping);
                if res.is_err() {
                    log::debug!("play_sound: found closed channel for {sound_name}");
                    data.sound_entries.remove(i);
                    break;
                }
                log::warn!("play_sound: Sound is already playing");
                return;
            } else {
                i += 1;
            }
        }
    }

    let sounds = hopp::sounds::get_all_sounds();
    let mut sound_path = "".to_string();
    let mut sound_config = SoundConfig::default();
    for sound in sounds {
        if sound.0.contains(&sound_name) {
            let resource_path = app.path().resolve(sound.0, BaseDirectory::Resource);
            if let Err(e) = resource_path {
                log::error!("play_sound: Failed to resolve sound path: {e:?}");
                return;
            }
            sound_path = resource_path.unwrap().to_string_lossy().to_string();
            sound_config = sound.1;
            break;
        }
    }
    if sound_path.is_empty() {
        log::error!("play_sound: Failed to find sound");
        return;
    }

    let (tx, rx) = std::sync::mpsc::channel();
    tauri::async_runtime::spawn(async move {
        let res = hopp::sounds::play_sound(sound_path, sound_config, rx);
        if res.is_err() {
            log::error!("play_sound: Failed to play sound: {:?}", res.err());
        }
    });

    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    data.sound_entries.push(sounds::SoundEntry {
        name: sound_name,
        tx,
    });
}

#[tauri::command(async)]
fn stop_sound(app: tauri::AppHandle, sound_name: String) {
    log::info!("stop_sound");
    let tmp_sound_name = sound_name.split("/").last();
    if let Some(tmp_sound_name) = tmp_sound_name {
        log::info!("Stopping sound: {}", tmp_sound_name);
    }
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    let mut i = 0;
    while i < data.sound_entries.len() {
        if data.sound_entries[i].name == sound_name {
            let _ = data.sound_entries[i].tx.send(sounds::SoundCommand::Stop);
            data.sound_entries.remove(i);
            break;
        } else {
            i += 1;
        }
    }
    log::debug!("stop_sound: entries left: {}", data.sound_entries.len());
}

#[tauri::command(async)]
fn reset_core_process(app: tauri::AppHandle) {
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    if let Err(e) = data.sender.send(Message::CallEnd) {
        log::error!("reset_core_process: failed to send message: {e:?}");
    }
}

#[tauri::command(async)]
fn store_token_cmd(app: tauri::AppHandle, token: String) {
    log::info!("store_token_cmd");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    data.app_state.set_user_jwt(Some(token.clone()));

    if let Err(e) = app.emit("token_changed", token) {
        log::error!("Failed to emit token_changed event: {e:?}");
    }
}

#[tauri::command(async)]
fn get_stored_token(app: tauri::AppHandle) -> Option<String> {
    log::info!("get_stored_token");
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    let token = data.app_state.user_jwt().clone();
    log::debug!("get_stored_token: {token:?}");
    token
}

#[tauri::command(async)]
fn delete_stored_token(app: tauri::AppHandle) {
    log::info!("Deleting stored token");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    data.app_state.set_user_jwt(None);

    if let Err(e) = app.emit("token_changed", "".to_string()) {
        log::error!("Failed to emit token_changed event: {e:?}");
    }
}

#[tauri::command(async)]
fn get_logs(_app: tauri::AppHandle) -> String {
    log::info!("get_logs:");
    let log_file = get_log_path();
    if let Some(path) = log_file {
        path.to_string_lossy().to_string()
    } else {
        log::error!("Failed to get log path");
        "".to_string()
    }
}

#[tauri::command(async)]
fn set_deactivate_hiding(app: tauri::AppHandle, deactivate: bool) {
    log::debug!("set_deactivate_hiding: {deactivate}");
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    let mut deactivate_hiding = data.deactivate_hiding.lock().unwrap();
    *deactivate_hiding = deactivate;
}

#[tauri::command(async)]
fn set_controller_cursor(app: tauri::AppHandle, enabled: bool) {
    log::info!("set_controller_cursor: {enabled}");
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    if let Err(e) = data.sender.send(Message::ControllerCursorEnabled(enabled)) {
        log::error!("set_controller_cursor: failed to send message: {e:?}");
    }
}

#[tauri::command(async)]
fn open_accessibility_settings(_app: tauri::AppHandle) {
    log::info!("open_accessibility_settings");
    let mut process = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
        .spawn()
        .expect("Failed to open System Preferences for Accessibility permissions");
    let _ = process.wait();
}

#[tauri::command(async)]
fn open_microphone_settings(_app: tauri::AppHandle) {
    log::info!("open_microphone_settings");
    permissions::request_microphone();
}

#[tauri::command(async)]
fn open_camera_settings(_app: tauri::AppHandle) {
    log::info!("open_camera_settings");
    permissions::request_camera();
}

#[tauri::command(async)]
fn open_screenshare_settings(_app: tauri::AppHandle) {
    log::info!("open_screenshare_settings");
    let mut process = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
        .spawn()
        .expect("Failed to open System Preferences for Screen Capture permissions");
    let _ = process.wait();
}

#[tauri::command(async)]
async fn trigger_screenshare_permission(_app: tauri::AppHandle) -> bool {
    log::info!("trigger_screenshare_permission");
    permissions::request_screenshare()
}

#[tauri::command(async)]
fn get_control_permission(_app: tauri::AppHandle) -> bool {
    let res = permissions::accessibility();
    log::info!("get_control_permission: {res}");
    res
}

#[tauri::command(async)]
fn get_microphone_permission(_app: tauri::AppHandle) -> bool {
    let res = permissions::microphone();
    log::info!("get_microphone_permission: {res}");
    res
}

#[tauri::command(async)]
fn get_screenshare_permission(_app: tauri::AppHandle) -> bool {
    let res = permissions::screenshare();
    log::info!("get_screenshare_permission: {res}");
    res
}

#[tauri::command(async)]
fn get_camera_permission(_app: tauri::AppHandle) -> bool {
    let res = permissions::camera();
    log::info!("get_camera_permission: {res}");
    res
}

#[tauri::command(async)]
fn skip_tray_notification_selection_window(app: tauri::AppHandle) {
    log::info!("executing skip_tray_notification_selection_window");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    data.app_state.set_tray_notification(false);
}

#[tauri::command(async)]
fn get_last_used_mic(app: tauri::AppHandle) -> Option<String> {
    log::info!("get_last_used_mic");
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    let value = data.app_state.last_used_mic();
    log::info!("get_last_used_mic: {value:?}");
    value
}

#[tauri::command(async)]
fn set_last_used_mic(app: tauri::AppHandle, mic: String) {
    log::info!("set_last_used_mic: {mic}");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    data.app_state.set_last_used_mic(mic);
}

#[tauri::command(async)]
fn get_last_used_camera(app: tauri::AppHandle) -> Option<String> {
    log::info!("get_last_used_camera");
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    let value = data.app_state.last_used_camera();
    log::info!("get_last_used_camera: {value:?}");
    value
}

#[tauri::command(async)]
fn set_last_used_camera(app: tauri::AppHandle, camera: String) {
    log::info!("set_last_used_camera: {camera}");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    data.app_state.set_last_used_camera(camera);
}

#[tauri::command(async)]
fn get_sharer_draw_persist(app: tauri::AppHandle) -> bool {
    log::info!("get_sharer_draw_persist");
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    let value = data.app_state.sharer_draw_persist();
    log::info!("get_sharer_draw_persist: {value}");
    value
}

/// The Settings option "Keep drawings until right-click": whether lines stay
/// until right-click or fade, both when drawing on your own shared screen and
/// on a teammate's.
#[tauri::command(async)]
fn set_draw_persist(app: tauri::AppHandle, persist: bool) {
    log::info!("set_draw_persist: {persist}");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    data.app_state.set_sharer_draw_persist(persist);
    data.app_state.set_controller_draw_persist(persist);

    if data.drawing_enabled {
        if let Err(e) = data.sender.send(Message::SharerDrawPersistChanged(persist)) {
            log::error!("set_draw_persist: failed to send SharerDrawPersistChanged: {e:?}");
        }
    }
    if let Err(e) = data
        .sender
        .send(Message::ControllerDrawPersistChanged(persist))
    {
        log::error!("set_draw_persist: failed to send ControllerDrawPersistChanged: {e:?}");
    }
}

#[tauri::command(async)]
fn get_drawing_hint_shown(app: tauri::AppHandle) -> bool {
    log::info!("get_drawing_hint_shown");
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    let value = data.app_state.drawing_hint_shown();
    log::info!("get_drawing_hint_shown: {value}");
    value
}

#[tauri::command(async)]
fn set_drawing_hint_shown(app: tauri::AppHandle, shown: bool) {
    log::info!("set_drawing_hint_shown: {shown}");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    data.app_state.set_drawing_hint_shown(shown);
}

#[tauri::command(async)]
fn get_drawing_enabled(app: tauri::AppHandle) -> bool {
    log::info!("get_drawing_enabled");
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    data.drawing_enabled
}

#[tauri::command(async)]
fn set_drawing_enabled(app: tauri::AppHandle, enabled: bool, permanent: bool) {
    apply_drawing_enabled(&app, enabled, permanent);
}

/// Turns the sharer's drawing on or off. Used by the call window's pen button
/// and the core's menu bar pen icon; tells the frontend through
/// `drawing_enabled_changed`.
fn apply_drawing_enabled(app: &tauri::AppHandle, enabled: bool, permanent: bool) {
    log::info!("set_drawing_enabled: enabled={enabled} permanent={permanent}");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();

    if data.drawing_enabled == enabled {
        return;
    }

    if enabled {
        if let Some(window) = app.get_webview_window("main") {
            #[cfg(target_os = "macos")]
            let _ = window.hide();
            #[cfg(target_os = "windows")]
            let _ = window.minimize();
        }
        // The drawing window lives in the core process; let it take over as
        // the active app so it gets the mouse and shows the pen cursor.
        #[cfg(target_os = "macos")]
        let _ = app.run_on_main_thread(yield_activation_to_core);
    }

    if let Err(e) = data
        .sender
        .send(Message::DrawingEnabled(DrawingEnabled { permanent }))
    {
        log::error!("set_drawing_enabled: failed to send message: {e:?}");
        return;
    }

    data.drawing_enabled = enabled;
    drop(data);

    #[cfg(not(target_os = "macos"))]
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.set_always_on_top(enabled);
    }
    if let Err(e) = app.emit("drawing_enabled_changed", enabled) {
        log::error!("set_drawing_enabled: failed to emit drawing_enabled_changed: {e:?}");
    }
}

#[tauri::command(async)]
fn quit_app(app: tauri::AppHandle) {
    log::info!("quit_app");
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    if let Err(e) = data.sender.send(Message::CallEnd) {
        log::error!("quit_app: failed to send CallEnd: {e:?}");
    }
    drop(data);
    app.exit(0);
}

#[tauri::command(async)]
fn minimize_main_window(app: tauri::AppHandle) {
    log::info!("minimize_main_window");
    if let Some(window) = app.get_webview_window("main") {
        if let Err(e) = window.minimize() {
            log::error!("Failed to minimize main window: {e:?}");
        }
    } else {
        log::error!("Main window not found");
    }
}

/// Hides the menu bar window once a 1:1 call is connected. Only on macOS,
/// where the main window is a menu bar popup; elsewhere it is the app window.
#[tauri::command(async)]
fn hide_main_window(app: tauri::AppHandle) {
    log::info!("hide_main_window");
    #[cfg(target_os = "macos")]
    if let Some(window) = app.get_webview_window("main") {
        if let Err(e) = window.hide() {
            log::error!("Failed to hide main window: {e:?}");
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = app;
}

#[tauri::command(async)]
fn set_livekit_url(app: tauri::AppHandle, url: String) {
    log::info!("set_livekit_url");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    if data.livekit_server_url != url {
        data.livekit_server_url = url.clone();
        if let Err(e) = data.sender.send(Message::LivekitServerUrl(url)) {
            log::error!("set_livekit_url: failed to send message: {e:?}");
        }
    }
}

#[tauri::command(async)]
fn get_livekit_url(app: tauri::AppHandle) -> String {
    log::info!("get_livekit_url");
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    data.livekit_server_url.clone()
}

#[tauri::command(async)]
fn set_sentry_metadata(app: tauri::AppHandle, user_id: String, app_version: String) {
    log::info!("set_sentry_metadata");
    sentry_utils::init_metadata(user_id.clone(), app_version.clone());
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    if let Err(e) = data.sender.send(Message::SentryMetadata(SentryMetadata {
        user_id,
        app_version,
    })) {
        log::error!("set_sentry_metadata: failed to send message: {e:?}");
    }
}

#[tauri::command(async)]
fn call_started(
    app: tauri::AppHandle,
    audio_token: String,
    video_token: String,
) -> Result<(), String> {
    log::info!("call_started");
    let data = app.state::<Mutex<AppData>>();
    #[allow(unused_mut)]
    let mut data = data.lock().unwrap();
    let user_settings = data.app_state.user_settings();
    // An empty name makes the core capture from the system default input and
    // follow it when it changes. Otherwise resolve preferred → last used →
    // system default → first → "".
    let audio_device_name = if user_settings.follows_system_default_mic() {
        String::new()
    } else {
        let last_used = data.app_state.last_used_mic();
        let devices: Vec<AudioDevice> = if let Err(e) = data.sender.send(Message::ListAudioDevices)
        {
            log::error!("call_started: failed to list audio devices: {e:?}");
            vec![]
        } else {
            match recv_expected_response(&data.event_socket, |msg| match msg {
                Message::AudioDeviceList(d) => Ok(d),
                other => Err(other),
            }) {
                Ok(d) => d,
                Err(e) => {
                    log::error!("call_started: failed to receive audio device list: {e:?}");
                    vec![]
                }
            }
        };
        resolve_microphone(
            &devices,
            user_settings.default_mic.as_deref(),
            last_used.as_deref(),
        )
    };
    log::info!("call_started: resolved audio_device_name={audio_device_name:?}");
    if let Err(e) = data
        .sender
        .send(Message::CallStart(socket_lib::CallStartMessage {
            audio_token: audio_token.clone(),
            video_token: video_token.clone(),
            audio_device_name,
            start_mic_on_call: Some(user_settings.start_mic_on_call),
            start_camera_on_call: Some(user_settings.start_camera_on_call),
        }))
    {
        log::error!("call_started: failed to send: {e:?}");
        return Err("Failed to send message to hopp_core".to_string());
    }
    let result = match recv_expected_response(&data.event_socket, |msg| match msg {
        Message::CallStartResult(r) => Ok(r),
        other => Err(other),
    }) {
        Ok(result) => result,
        Err(e) => {
            log::error!("call_started: recv failed: {e:?}");
            Err("Failed to receive message from hopp_core".to_string())
        }
    };
    if result.is_ok() {
        data.call_active = true;
        data.is_camera_on = false;
        data.is_screensharing = false;
        shortcuts::register_call_shortcuts(&app, resolved_call_shortcuts(&data.app_state));
    }
    result
}

/// When enabled=true, shows the notification variant of the icon.
/// When enabled=false, shows the default variant.
///
/// NOTE: must NOT be `async`. The macOS implementation manipulates AppKit/CALayer,
/// which has to run on the main thread; an async command would run off-thread and
/// the notification dot would silently never update.
#[tauri::command]
fn set_tray_notification(app: tauri::AppHandle, enabled: bool) {
    log::info!("set_tray_notification: enabled={}", enabled);
    let data = app.state::<std::sync::Mutex<hopp::AppData>>();
    let mut data = data.lock().unwrap();
    if let Some(ref mut tray) = data.tray_state {
        tray.set_notification_enabled(enabled);
    }
}

#[tauri::command(async)]
fn get_hopp_server_url(app: tauri::AppHandle) -> Option<String> {
    log::info!("get_hopp_server_url");
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    let url = data.app_state.user_settings().hopp_server_url;
    log::debug!("get_hopp_server_url: {url:?}");
    url
}

#[tauri::command(async)]
fn set_hopp_server_url(app: tauri::AppHandle, url: Option<String>) {
    log::info!("set_hopp_server_url: {url:?}");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    data.app_state
        .update_user_setting(|s| s.hopp_server_url = url.clone());
    let _ = app.emit("hopp_server_url_changed", &url);
}

#[tauri::command(async)]
async fn create_feedback_window(
    app: tauri::AppHandle,
    team_id: String,
    room_id: String,
    participant_id: String,
) -> Result<(), String> {
    log::info!("create_feedback_window");

    let url = format!(
        "feedback.html?teamId={}&roomId={}&participantId={}",
        team_id, room_id, participant_id
    );
    hopp::create_media_window(
        &app,
        hopp::MediaWindowConfig {
            label: "feedback",
            title: "Call Feedback",
            url: &url,
            width: 500.0,
            height: 420.0,
            resizable: false,
            always_on_top: true,
            content_protected: false,
            maximizable: false,
            minimizable: false,
            decorations: true,
            transparent: false,
            background_color: Some(tauri::webview::Color(0, 0, 0, 0)),
        },
    )
}

#[tauri::command(async)]
async fn create_settings_window(app: tauri::AppHandle) -> Result<(), String> {
    log::info!("create_settings_window");
    // Settings is a regular window: in the Dock and in Cmd+Tab while open.
    #[cfg(target_os = "macos")]
    {
        app.state::<Mutex<AppData>>()
            .lock()
            .unwrap()
            .settings_window_open = true;
        sync_dock_icon(&app);
    }
    hopp::create_media_window(
        &app,
        hopp::MediaWindowConfig {
            label: "settings",
            title: "Settings",
            url: "settings.html",
            width: 800.0,
            height: 840.0,
            resizable: false,
            always_on_top: false,
            content_protected: false,
            maximizable: false,
            minimizable: true,
            decorations: true,
            transparent: false,
            background_color: None,
        },
    )
}

fn resolved_call_shortcuts(app_state: &hopp::app_state::AppState) -> shortcuts::CallShortcuts {
    let mut settings = app_state.user_settings();
    settings.resolve_shortcuts();
    shortcuts::CallShortcuts {
        mic: settings.shortcut_toggle_mic.unwrap_or_default(),
        camera: settings.shortcut_toggle_camera.unwrap_or_default(),
        screenshare: settings.shortcut_toggle_screenshare.unwrap_or_default(),
        end_call: settings.shortcut_end_call.unwrap_or_default(),
    }
}

#[tauri::command(async)]
fn get_user_settings(app: tauri::AppHandle) -> UserSettings {
    log::info!("get_user_settings");
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    let mut settings = data.app_state.user_settings();
    settings.resolve_shortcuts();
    settings
}

#[tauri::command(async)]
fn list_installed_applications() -> Result<Vec<InstalledApplication>, String> {
    Ok(hopp::application_catalog::list_installed_applications())
}

#[tauri::command(async)]
fn set_app_veil_applications(
    app: tauri::AppHandle,
    applications: Vec<AppVeilApplication>,
) -> Result<(), String> {
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    let enabled_bundle_ids = data.app_state.set_app_veil_applications(applications)?;
    data.sender
        .send(Message::SetAppVeilBundleIds(enabled_bundle_ids))
        .map_err(|error| {
            log::error!("set_app_veil_applications: failed to send settings: {error:?}");
            sentry_utils::simple_event(format!(
                "Failed to send App Veil settings to core: {error}"
            ));
            "App Veil was saved but could not be applied until Hopp restarts".to_string()
        })
}

#[tauri::command(async)]
fn set_shortcut_toggle_mic(app: tauri::AppHandle, accel: String) {
    log::info!("set_shortcut_toggle_mic: {accel}");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    let value = if accel.is_empty() { None } else { Some(accel) };
    data.app_state
        .update_user_setting(|s| s.shortcut_toggle_mic = value);
    if data.call_active {
        shortcuts::unregister_call_shortcuts(&app);
        shortcuts::register_call_shortcuts(&app, resolved_call_shortcuts(&data.app_state));
    }
}

#[tauri::command(async)]
fn set_shortcut_toggle_camera(app: tauri::AppHandle, accel: String) {
    log::info!("set_shortcut_toggle_camera: {accel}");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    let value = if accel.is_empty() { None } else { Some(accel) };
    data.app_state
        .update_user_setting(|s| s.shortcut_toggle_camera = value);
    if data.call_active {
        shortcuts::unregister_call_shortcuts(&app);
        shortcuts::register_call_shortcuts(&app, resolved_call_shortcuts(&data.app_state));
    }
}

#[tauri::command(async)]
fn set_shortcut_toggle_screenshare(app: tauri::AppHandle, accel: String) {
    log::info!("set_shortcut_toggle_screenshare: {accel}");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    let value = if accel.is_empty() { None } else { Some(accel) };
    data.app_state
        .update_user_setting(|s| s.shortcut_toggle_screenshare = value);
    if data.call_active {
        shortcuts::unregister_call_shortcuts(&app);
        shortcuts::register_call_shortcuts(&app, resolved_call_shortcuts(&data.app_state));
    }
}

#[tauri::command(async)]
fn set_shortcut_end_call(app: tauri::AppHandle, accel: String) {
    log::info!("set_shortcut_end_call: {accel}");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    let value = if accel.is_empty() { None } else { Some(accel) };
    data.app_state
        .update_user_setting(|s| s.shortcut_end_call = value);
    if data.call_active {
        shortcuts::unregister_call_shortcuts(&app);
        shortcuts::register_call_shortcuts(&app, resolved_call_shortcuts(&data.app_state));
    }
}

#[tauri::command(async)]
fn set_is_camera_on(app: tauri::AppHandle, value: bool) {
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    data.is_camera_on = value;
}

#[tauri::command(async)]
fn set_is_screensharing(app: tauri::AppHandle, value: bool) {
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    data.is_screensharing = value;
}

#[tauri::command(async)]
fn set_call_feedback_popup(app: tauri::AppHandle, enabled: bool) {
    log::info!("set_call_feedback_popup: {enabled}");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    data.app_state
        .update_user_setting(|s| s.call_feedback_popup = enabled);
}

#[tauri::command(async)]
fn set_telemetry_enabled(app: tauri::AppHandle, enabled: bool) {
    log::info!("set_telemetry_enabled: {enabled}");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    data.app_state
        .update_user_setting(|s| s.telemetry_enabled = enabled);
    sentry_utils::set_telemetry_enabled(enabled);
    if let Err(e) = data.sender.send(Message::SetTelemetryEnabled(enabled)) {
        log::error!("set_telemetry_enabled: failed to send: {e:?}");
    }
    let _ = app.emit("telemetry_enabled_changed", enabled);
}

#[tauri::command(async)]
fn set_show_dock_icon_in_call(app: tauri::AppHandle, enabled: bool) {
    log::info!("set_show_dock_icon_in_call: {enabled}");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    data.app_state
        .update_user_setting(|s| s.show_dock_icon_in_call = enabled);
}

#[tauri::command(async)]
fn set_notify_when_alone(app: tauri::AppHandle, enabled: bool) {
    log::info!("set_notify_when_alone: {enabled}");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    data.app_state
        .update_user_setting(|s| s.notify_when_alone = enabled);
}

#[tauri::command(async)]
fn set_auto_update_enabled(app: tauri::AppHandle, enabled: bool) {
    log::info!("set_auto_update_enabled: {enabled}");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    data.app_state
        .update_user_setting(|s| s.auto_update_enabled = enabled);
}

#[tauri::command(async)]
fn set_start_camera_on_call(app: tauri::AppHandle, enabled: bool) {
    log::info!("set_start_camera_on_call: {enabled}");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    data.app_state
        .update_user_setting(|s| s.start_camera_on_call = enabled);
}

#[tauri::command(async)]
fn set_start_mic_on_call(app: tauri::AppHandle, enabled: bool) {
    log::info!("set_start_mic_on_call: {enabled}");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    data.app_state
        .update_user_setting(|s| s.start_mic_on_call = enabled);
}

#[tauri::command(async)]
fn set_remote_control_enabled(app: tauri::AppHandle, enabled: bool) {
    log::info!("set_remote_control_enabled: {enabled}");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    data.app_state
        .update_user_setting(|s| s.remote_control_enabled = enabled);
    drop(data);
    set_controller_cursor(app, enabled);
}

#[tauri::command(async)]
fn mute_mic(app: tauri::AppHandle) {
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    if let Err(e) = data.sender.send(Message::MuteAudio) {
        log::error!("mute_mic: failed to send: {e:?}");
    }
}

#[tauri::command(async)]
fn unmute_mic(app: tauri::AppHandle) {
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    if let Err(e) = data.sender.send(Message::UnmuteAudio) {
        log::error!("unmute_mic: failed to send: {e:?}");
    }
}

#[tauri::command(async)]
fn toggle_mic(app: tauri::AppHandle) {
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    if let Err(e) = data.sender.send(Message::ToggleMic) {
        log::error!("toggle_mic: failed to send: {e:?}");
    }
}

#[tauri::command(async)]
fn set_noise_cancellation(app: tauri::AppHandle, enabled: bool) {
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    data.app_state
        .update_user_setting(|settings| settings.noise_cancellation_enabled = enabled);
    if let Err(e) = data.sender.send(Message::SetNoiseCancellation(enabled)) {
        log::error!("set_noise_cancellation: failed to send: {e:?}");
    }
}

#[tauri::command(async)]
fn set_screen_share_resolution(app: tauri::AppHandle, resolution: ScreenShareResolution) {
    log::info!("set_screen_share_resolution: {resolution:?}");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    data.app_state
        .update_user_setting(|settings| settings.screen_share_resolution = resolution);
    if let Err(e) = data
        .sender
        .send(Message::SetScreenShareResolution(resolution))
    {
        log::error!("set_screen_share_resolution: failed to send: {e:?}");
    }
}

#[tauri::command(async)]
fn set_screen_share_picker_mode(app: tauri::AppHandle, mode: ScreenSharePickerMode) {
    log::info!("set_screen_share_picker_mode: {mode:?}");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    data.app_state
        .update_user_setting(|settings| settings.screen_share_picker_mode = mode);
    if let Err(e) = data.sender.send(Message::SetScreenSharePickerMode(mode)) {
        log::error!("set_screen_share_picker_mode: failed to send: {e:?}");
    }
}

#[tauri::command(async)]
fn start_camera(app: tauri::AppHandle, device_name: Option<String>) -> Result<(), String> {
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    if let Err(e) = data
        .sender
        .send(Message::StartCamera(socket_lib::CameraStartMessage {
            device_name,
        }))
    {
        log::error!("start_camera: failed to send: {e:?}");
        return Err("Failed to send message to hopp_core".to_string());
    }
    match recv_expected_response(&data.event_socket, |msg| match msg {
        Message::StartCameraResult(r) => Ok(r),
        other => Err(other),
    }) {
        Ok(result) => result,
        Err(e) => {
            log::error!("start_camera: recv failed: {e:?}");
            Err("Failed to receive message from hopp_core".to_string())
        }
    }
}

#[tauri::command(async)]
fn stop_camera(app: tauri::AppHandle) {
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    if let Err(e) = data.sender.send(Message::StopCamera) {
        log::error!("stop_camera: failed to send: {e:?}");
    }
}

#[tauri::command(async)]
fn open_camera_preview(app: tauri::AppHandle) {
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    if let Err(e) = data.sender.send(Message::OpenCamera) {
        log::error!("open_camera_preview: failed to send: {e:?}");
    }
}

#[tauri::command(async)]
fn open_screenshare_viewer(app: tauri::AppHandle) {
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    if let Err(e) = data.sender.send(Message::OpenScreenShareWindow) {
        log::error!("open_screenshare_viewer: failed to send: {e:?}");
    }
}

#[tauri::command(async)]
fn close_screenshare_viewer(app: tauri::AppHandle) {
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    if let Err(e) = data.sender.send(Message::CloseScreenShareWindow) {
        log::error!("close_screenshare_viewer: failed to send: {e:?}");
    }
}

#[tauri::command(async)]
fn list_microphones(app: tauri::AppHandle) -> Vec<AudioDevice> {
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    if let Err(e) = data.sender.send(Message::ListAudioDevices) {
        log::error!("list_microphones: failed to send: {e:?}");
        return vec![];
    }
    match recv_expected_response(&data.event_socket, |msg| match msg {
        Message::AudioDeviceList(d) => Ok(d),
        other => Err(other),
    }) {
        Ok(devices) => devices,
        Err(e) => {
            log::error!("list_microphones: recv failed: {e:?}");
            vec![]
        }
    }
}

#[tauri::command(async)]
fn list_speakers(app: tauri::AppHandle) -> Vec<AudioDevice> {
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    if let Err(e) = data.sender.send(Message::ListAudioOutputs) {
        log::error!("list_speakers: failed to send: {e:?}");
        return vec![];
    }
    match recv_expected_response(&data.event_socket, |msg| match msg {
        Message::AudioOutputList(d) => Ok(d),
        other => Err(other),
    }) {
        Ok(devices) => devices,
        Err(e) => {
            log::error!("list_speakers: recv failed: {e:?}");
            vec![]
        }
    }
}

/// Plays call audio on `device_name`, or the system default output when `None`.
/// Applies right away, also mid-call.
#[tauri::command(async)]
fn set_audio_output(app: tauri::AppHandle, device_name: Option<String>) {
    log::info!("set_audio_output: {device_name:?}");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    data.app_state
        .update_user_setting(|s| s.audio_output = device_name.clone());
    if let Err(e) = data.sender.send(Message::SetAudioOutput(device_name)) {
        log::error!("set_audio_output: failed to send: {e:?}");
    }
}

/// Sends the core what it needs to transcribe calls, or that it shouldn't.
fn sync_transcription(app: &tauri::AppHandle) {
    let Ok(app_data_dir) = app.path().app_data_dir() else {
        log::error!("sync_transcription: no app data dir");
        return;
    };
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    let settings =
        hopp::transcription::core_settings(&app_data_dir, &data.app_state.user_settings());
    if let Err(e) = data.sender.send(Message::SetTranscription(settings)) {
        log::error!("sync_transcription: failed to send: {e:?}");
    }
}

#[tauri::command(async)]
fn get_transcription_info(
    app: tauri::AppHandle,
) -> Result<hopp::transcription::TranscriptionInfo, String> {
    let app_data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    Ok(hopp::transcription::info(&app_data_dir))
}

#[tauri::command]
async fn download_transcription_model(app: tauri::AppHandle) -> Result<(), String> {
    let app_data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    hopp::transcription::download_model(&app, &app_data_dir).await?;
    sync_transcription(&app);
    Ok(())
}

#[tauri::command(async)]
fn set_transcription_enabled(app: tauri::AppHandle, enabled: bool) {
    log::info!("set_transcription_enabled: {enabled}");
    {
        let data = app.state::<Mutex<AppData>>();
        let mut data = data.lock().unwrap();
        data.app_state
            .update_user_setting(|s| s.transcription_enabled = enabled);
    }
    sync_transcription(&app);
}

#[tauri::command(async)]
fn set_transcription_language(app: tauri::AppHandle, language: String) {
    log::info!("set_transcription_language: {language}");
    {
        let data = app.state::<Mutex<AppData>>();
        let mut data = data.lock().unwrap();
        data.app_state
            .update_user_setting(|s| s.transcription_language = language);
    }
    sync_transcription(&app);
}

#[tauri::command(async)]
fn open_transcripts_folder(app: tauri::AppHandle) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let app_data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let dir = hopp::transcription::transcripts_dir(&app_data_dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    app.opener()
        .open_path(dir.to_string_lossy(), None::<&str>)
        .map_err(|e| e.to_string())
}

#[tauri::command(async)]
fn set_share_system_audio(app: tauri::AppHandle, enabled: bool) {
    log::info!("set_share_system_audio: {enabled}");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    data.app_state
        .update_user_setting(|s| s.share_system_audio = enabled);
    if let Err(e) = data.sender.send(Message::SetShareSystemAudio(enabled)) {
        log::error!("set_share_system_audio: failed to send: {e:?}");
    }
}

#[tauri::command(async)]
fn set_open_received_links(app: tauri::AppHandle, enabled: bool) {
    log::info!("set_open_received_links: {enabled}");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    data.app_state
        .update_user_setting(|s| s.open_received_links = enabled);
}

/// A viewer sent a link while the user shares their screen: open it in the
/// default browser, or copy it and say so, as Settings say. Only http(s).
fn handle_received_link(app: &tauri::AppHandle, link: socket_lib::ReceivedLink) {
    use tauri_plugin_clipboard_manager::ClipboardExt;
    use tauri_plugin_notification::NotificationExt;
    use tauri_plugin_opener::OpenerExt;

    let is_web_link = tauri::Url::parse(&link.url)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some());
    if !is_web_link {
        log::warn!("handle_received_link: ignoring a link that isn't http(s)");
        return;
    }
    let sender = if link.sender_name.is_empty() {
        "Your pair".to_string()
    } else {
        link.sender_name.clone()
    };
    let open = {
        let data = app.state::<Mutex<AppData>>();
        let data = data.lock().unwrap();
        data.app_state.user_settings().open_received_links
    };
    if open {
        log::info!("handle_received_link: opening a link from {sender}");
        if let Err(e) = app.opener().open_url(&link.url, None::<&str>) {
            log::error!("handle_received_link: failed to open the link: {e:?}");
        }
        return;
    }
    log::info!("handle_received_link: copying a link from {sender}");
    if let Err(e) = app.clipboard().write_text(link.url.clone()) {
        log::error!("handle_received_link: failed to copy the link: {e:?}");
        return;
    }
    if let Err(e) = app
        .notification()
        .builder()
        .title(format!("{sender} sent you a link"))
        .body(format!("Copied to your clipboard: {}", link.url))
        .show()
    {
        log::error!("handle_received_link: failed to notify: {e:?}");
    }
}

#[tauri::command(async)]
fn set_webcam_preview(app: tauri::AppHandle, enabled: bool) {
    log::info!("set_webcam_preview: {enabled}");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    data.app_state
        .update_user_setting(|s| s.webcam_preview = enabled);
    if let Err(e) = data.sender.send(Message::SetWebcamPreview(enabled)) {
        log::error!("set_webcam_preview: failed to send: {e:?}");
    }
}

#[tauri::command(async)]
fn set_auto_mic_volume(app: tauri::AppHandle, enabled: bool) {
    log::info!("set_auto_mic_volume: {enabled}");
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    data.app_state
        .update_user_setting(|s| s.auto_mic_volume = enabled);
    if let Err(e) = data.sender.send(Message::SetAutoMicVolume(enabled)) {
        log::error!("set_auto_mic_volume: failed to send: {e:?}");
    }
}

#[tauri::command(async)]
fn set_default_mic(
    app: tauri::AppHandle,
    device_name: Option<String>,
    use_last_used: bool,
) -> Result<(), String> {
    let data = app.state::<Mutex<AppData>>();
    let mut data = data.lock().unwrap();
    data.app_state.set_default_mic(device_name, use_last_used)
}

#[tauri::command(async)]
fn get_preferred_microphone(app: tauri::AppHandle) -> String {
    let devices = list_microphones(app.clone());
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    let settings = data.app_state.user_settings();
    if settings.follows_system_default_mic() {
        // The device flagged `default` is the system default input.
        return resolve_microphone(&devices, None, None);
    }
    resolve_microphone(
        &devices,
        settings.default_mic.as_deref(),
        data.app_state.last_used_mic().as_deref(),
    )
}

#[tauri::command(async)]
fn select_microphone(app: tauri::AppHandle, device_name: String) {
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    if let Err(e) = data
        .sender
        .send(Message::StartAudioCapture(AudioCaptureMessage {
            device_name,
        }))
    {
        log::error!("select_microphone: failed to send: {e:?}");
        return;
    }
    match recv_expected_response(&data.event_socket, |msg| match msg {
        Message::StartAudioCaptureResult(r) => Ok(r),
        other => Err(other),
    }) {
        Ok(Err(e)) => log::error!("select_microphone: core failed: {e}"),
        Err(e) => log::error!("select_microphone: no result: {e:?}"),
        Ok(Ok(())) => {}
    }
}

#[tauri::command(async)]
fn list_webcams(app: tauri::AppHandle) -> Vec<CameraDevice> {
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    if let Err(e) = data.sender.send(Message::ListCameras) {
        log::error!("list_webcams: failed to send: {e:?}");
        return vec![];
    }
    match recv_expected_response(&data.event_socket, |msg| match msg {
        Message::CameraList(d) => Ok(d),
        other => Err(other),
    }) {
        Ok(devices) => devices,
        Err(e) => {
            log::error!("list_webcams: recv failed: {e:?}");
            vec![]
        }
    }
}

#[tauri::command(async)]
fn bring_windows_to_front(app: tauri::AppHandle) -> bool {
    log::info!("bring_windows_to_front");
    let data = app.state::<Mutex<AppData>>();
    let data = data.lock().unwrap();
    if let Err(e) = data.sender.send(Message::BringWindowsToFront) {
        log::error!("bring_windows_to_front: failed to send: {e:?}");
        return false;
    }
    match recv_expected_response(&data.event_socket, |msg| match msg {
        Message::BringWindowsToFrontResult(f) => Ok(f),
        other => Err(other),
    }) {
        Ok(focused) => focused,
        Err(e) => {
            log::error!("bring_windows_to_front: recv failed: {e:?}");
            false
        }
    }
}

/// Turns Hopp back into a menu bar app (no Dock icon) after a call.
///
/// macOS takes focus away from the window during that switch, so hiding on
/// focus loss is paused for 300 ms to keep the window open. If the window had
/// focus before, it gets it back afterwards: a window that is already unfocused
/// can't lose focus again, so clicking elsewhere would no longer hide it.
///
/// Call without holding the `AppData` lock: window queries wait on the main thread.
#[cfg(target_os = "macos")]
fn return_to_menu_bar_mode(app: &tauri::AppHandle, suppress_hide: Arc<AtomicBool>) {
    let main_window = app.get_webview_window("main");
    let had_focus = main_window
        .as_ref()
        .is_some_and(|window| window.is_focused().unwrap_or(false));
    suppress_hide.store(true, Ordering::Relaxed);
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        suppress_hide.store(false, Ordering::Relaxed);
        let Some(window) = main_window.filter(|_| had_focus) else {
            return;
        };
        if window.is_visible().unwrap_or(false) && !window.is_focused().unwrap_or(true) {
            log::info!("return_to_menu_bar_mode: giving the main window its focus back");
            // Showing makes the panel key again without activating Hopp.
            let _ = window.show();
        }
    });
}

/// Hopp is a menu bar app. It shows a Dock icon, and so takes part in Cmd+Tab,
/// only while one of its windows is meant to be switched to: Settings, or a
/// teammate's shared screen (when that setting is on).
#[cfg(target_os = "macos")]
fn wants_dock_icon(data: &AppData) -> bool {
    data.settings_window_open
        || (data.screen_share_window_shown && data.app_state.user_settings().show_dock_icon_in_call)
}

/// Shows or hides the Dock icon to match `wants_dock_icon`.
///
/// Call without holding the `AppData` lock: it takes the lock itself.
#[cfg(target_os = "macos")]
fn sync_dock_icon(app: &tauri::AppHandle) {
    let state = app.state::<Mutex<AppData>>();
    let mut data = state.lock().unwrap();
    let wanted = wants_dock_icon(&data);
    if wanted == data.activation_policy_regular {
        return;
    }
    data.activation_policy_regular = wanted;
    if wanted {
        drop(data);
        let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    } else {
        let suppress = data.suppress_hide_on_call_end.clone();
        drop(data);
        return_to_menu_bar_mode(app, suppress);
    }
}

/// Hopp lives in the menu bar, so Cmd+Q closes the window in front instead of
/// quitting. The main window hides, as when clicking outside it.
#[cfg(target_os = "macos")]
fn close_focused_window(app: &tauri::AppHandle) {
    let focused = app
        .webview_windows()
        .into_values()
        .find(|window| window.is_focused().unwrap_or(false));
    match focused {
        Some(window) if window.label() == "main" => {
            let _ = window.hide();
        }
        Some(window) => {
            let _ = window.close();
        }
        None => {}
    }
}

/// macOS app menu: the standard items, with Cmd+Q bound to closing the front
/// window (see `close_focused_window`) and Quit kept without a shortcut. The
/// Edit menu keeps copy/paste working in text fields.
#[cfg(target_os = "macos")]
fn build_app_menu(app: &tauri::AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::SubmenuBuilder;

    let close_window = MenuItemBuilder::with_id(CLOSE_WINDOW_MENU_ID, "Close Window")
        .accelerator("Cmd+Q")
        .build(app)?;
    // Same id as the tray menu's Quit, whose handler exits the app.
    let quit = MenuItemBuilder::with_id("quit", "Quit Hopp").build(app)?;
    let app_menu = SubmenuBuilder::new(app, "Hopp")
        .about(None)
        .separator()
        .hide()
        .hide_others()
        .show_all()
        .separator()
        .item(&close_window)
        .item(&quit)
        .build()?;
    let edit_menu = SubmenuBuilder::new(app, "Edit")
        .undo()
        .redo()
        .separator()
        .cut()
        .copy()
        .paste()
        .select_all()
        .build()?;
    MenuBuilder::new(app)
        .items(&[&app_menu, &edit_menu])
        .build()
}

#[cfg(target_os = "macos")]
const CLOSE_WINDOW_MENU_ID: &str = "close_window";

#[tauri::command(async)]
fn end_call(app: tauri::AppHandle) {
    let state = app.state::<Mutex<AppData>>();
    #[allow(unused_mut)]
    let mut data = state.lock().unwrap();
    if let Err(e) = data.sender.send(Message::CallEnd) {
        log::error!("end_call: failed to send: {e:?}");
    }
    data.call_active = false;
    data.is_camera_on = false;
    data.is_screensharing = false;
    #[cfg(target_os = "macos")]
    {
        data.sleep_prevention.disable();
        data.screen_share_window_shown = false;
        drop(data);
        sync_dock_icon(&app);
    }
    shortcuts::unregister_call_shortcuts(&app);
}

#[tauri::command(async)]
fn toggle_call_sleep_prevention(app: tauri::AppHandle, enabled: bool) {
    #[cfg(target_os = "macos")]
    {
        let data = app.state::<Mutex<AppData>>();
        let mut data = data.lock().unwrap();
        if enabled {
            data.sleep_prevention.enable();
        } else {
            data.sleep_prevention.disable();
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, enabled);
    }
}

fn forward_core_events(events_rx: std_mpsc::Receiver<Message>, app: tauri::AppHandle) {
    log::info!("forward_core_events: starting event forwarding thread");
    for message in events_rx.iter() {
        match message {
            Message::ParticipantsSnapshot(snapshot) => {
                log::info!(
                    "forward_core_events: participants snapshot ({} participants)",
                    snapshot.len()
                );
                if let Err(e) = app.emit("core_participants_snapshot", &snapshot) {
                    log::error!("forward_core_events: failed to emit participants snapshot: {e:?}");
                }
            }
            Message::RoleChange(event) => {
                log::info!("forward_core_events: role change: {event:?}");
                if let Err(e) = app.emit("core_role_change", &event) {
                    log::error!("forward_core_events: failed to emit role change: {e:?}");
                }
            }
            Message::CameraFailed(error) => {
                log::error!("forward_core_events: camera failed: {error}");
                if let Err(e) = app.emit("core_camera_failed", &error) {
                    log::error!("forward_core_events: failed to emit camera failed: {e:?}");
                }
            }
            Message::CallEnded => {
                log::info!("forward_core_events: call ended");
                let data = app.state::<Mutex<AppData>>();
                let mut data = data.lock().unwrap();
                data.call_active = false;
                data.is_camera_on = false;
                data.is_screensharing = false;
                #[cfg(target_os = "macos")]
                {
                    data.sleep_prevention.disable();
                    data.screen_share_window_shown = false;
                    drop(data);
                    sync_dock_icon(&app);
                }
                #[cfg(not(target_os = "macos"))]
                drop(data);
                shortcuts::unregister_call_shortcuts(&app);
                if let Err(e) = app.emit("core_call_ended", &()) {
                    log::error!("forward_core_events: failed to emit call ended: {e:?}");
                }
            }
            Message::ScreenShareWindowShown(shown) => {
                log::info!("forward_core_events: screen share window shown: {shown}");
                // The Dock icon (and Cmd+Tab) is there only while a teammate's
                // screen is on view, the one Hopp window it can bring forward.
                #[cfg(target_os = "macos")]
                {
                    app.state::<Mutex<AppData>>()
                        .lock()
                        .unwrap()
                        .screen_share_window_shown = shown;
                    sync_dock_icon(&app);
                }
            }
            Message::ControllerDrawPersistChanged(persist) => {
                log::info!("forward_core_events: controller draw persist changed: {persist}");
                let data = app.state::<Mutex<AppData>>();
                let mut data = data.lock().unwrap();
                data.app_state.set_controller_draw_persist(persist);
            }
            Message::LastModeChanged(mode) => {
                log::info!("forward_core_events: last mode changed: {mode:?}");
                let data = app.state::<Mutex<AppData>>();
                let mut data = data.lock().unwrap();
                data.app_state.set_last_mode(mode);
            }
            Message::RoomConnectionFailed(reason) => {
                log::error!("forward_core_events: room connection failed: {reason}");
                if let Err(e) = app.emit("core_room_connection_failed", &reason) {
                    log::error!(
                        "forward_core_events: failed to emit room connection failed: {e:?}"
                    );
                }
            }
            Message::AppVeilFailed(reason) => {
                log::error!("forward_core_events: app veil failed: {reason}");
                if let Err(e) = app.emit("core_app_veil_failed", &reason) {
                    log::error!("forward_core_events: failed to emit app veil failed: {e:?}");
                }
            }
            Message::QueryPreferredCamera => {
                log::info!("forward_core_events: query preferred camera");
                let data = app.state::<Mutex<AppData>>();
                let data = data.lock().unwrap();
                let preferred = data.app_state.last_used_camera();
                if let Err(e) = data.sender.send(Message::PreferredCamera(preferred)) {
                    log::error!("forward_core_events: failed to send preferred camera: {e:?}");
                }
            }
            Message::ActiveMicChanged(device_name) => {
                log::info!("forward_core_events: active mic changed to: {device_name}");
                let data = app.state::<Mutex<AppData>>();
                let mut data = data.lock().unwrap();
                data.app_state.set_last_used_mic(device_name.clone());
                drop(data);
                if let Err(e) = app.emit("core_active_mic_changed", &device_name) {
                    log::error!(
                        "forward_core_events: failed to emit core_active_mic_changed: {e:?}"
                    );
                }
            }
            Message::LinkReceived(link) => handle_received_link(&app, link),
            Message::ActiveCameraChanged(device_name) => {
                log::info!("forward_core_events: active camera changed to: {device_name}");
                let data = app.state::<Mutex<AppData>>();
                let mut data = data.lock().unwrap();
                data.app_state.set_last_used_camera(device_name.clone());
                drop(data);
                if let Err(e) = app.emit("core_active_camera_changed", &device_name) {
                    log::error!(
                        "forward_core_events: failed to emit core_active_camera_changed: {e:?}"
                    );
                }
            }
            Message::MicrophoneAudioLevel(level) => {
                if let Err(e) = app.emit("core_mic_audio_level", &level) {
                    log::error!("forward_core_events: failed to emit mic audio level: {e:?}");
                }
            }
            Message::StartScreenShareResult(Err(error)) => {
                log::error!("forward_core_events: screen share failed: {error}");
                if let Err(e) = app.emit("core_screenshare_failed", &error) {
                    log::error!("forward_core_events: failed to emit screen share failed: {e:?}");
                }
            }
            Message::StartScreenShareResult(Ok(())) => {
                log::info!("forward_core_events: screen share started");
            }
            Message::DrawingToggleRequested => {
                let (enabled, permanent) = {
                    let data = app.state::<Mutex<AppData>>();
                    let data = data.lock().unwrap();
                    (data.drawing_enabled, data.app_state.sharer_draw_persist())
                };
                log::info!("forward_core_events: menu bar pen clicked (drawing was {enabled})");
                apply_drawing_enabled(&app, !enabled, permanent);
            }
            Message::DrawingDisabled => {
                log::info!("forward_core_events: drawing disabled");
                let data = app.state::<Mutex<AppData>>();
                let mut data = data.lock().unwrap();
                data.drawing_enabled = false;
                drop(data);
                #[cfg(not(target_os = "macos"))]
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.set_always_on_top(false);
                }
                if let Err(e) = app.emit("core_drawing_disabled", &()) {
                    log::error!("forward_core_events: failed to emit core_drawing_disabled: {e:?}");
                }
            }
            Message::ExitRequested => {
                log::info!("forward_core_events: exit requested from core");
                let data = app.state::<Mutex<AppData>>();
                let data = data.lock().unwrap();
                if let Err(e) = data.sender.send(Message::CallEnd) {
                    log::error!("forward_core_events: failed to send CallEnd: {e:?}");
                }
                drop(data);
                app.exit(0);
            }
            other => {
                log::error!("forward_core_events: unhandled event: {other:?}");
            }
        }
    }
    log::info!("forward_core_events: event forwarding thread exiting");
}

fn main() {
    let _guard = sentry_utils::init_sentry("Tauri backend".to_string(), Some(get_sentry_dsn()));

    /*
     * Flag for disabling hiding the window on focus lost.
     * This is used to prevent the window from hiding when the user is writing feedback.
     */
    let deactivate_hiding = Arc::new(Mutex::new(false));
    let deactivate_hiding_clone = deactivate_hiding.clone();

    /*
     * Flag for disabling hiding the window on focus lost.
     * This is used to prevent the window from hiding when the user uses Raycast/Spotlight
     * to open the app again.
     */
    let reopen_requested = Arc::new(Mutex::new(false));
    #[allow(unused_variables)]
    let reopen_requested_clone = reopen_requested.clone();

    /* This is used to guard against showing the main window if the location is not set. */
    #[allow(unused_variables)]
    let location_set = Arc::new(Mutex::new(false));
    #[allow(unused_variables)]
    let location_set_clone = location_set.clone();
    #[allow(unused_variables)]
    let location_set_setup = location_set.clone();

    /* Flag set during tray icon clicks to suppress spurious activation events. */
    let tray_clicked = Arc::new(AtomicBool::new(false));

    /* Flag to suppress main window hide when activation policy switches to Accessory after a call ends. */
    let suppress_hide_on_call_end = Arc::new(AtomicBool::new(false));
    let suppress_hide_on_call_end_clone = suppress_hide_on_call_end.clone();

    let log_level = get_log_level();
    let mut app = tauri::Builder::default().plugin(tauri_plugin_opener::init());
    if !cfg!(debug_assertions) {
        app = app.plugin(tauri_plugin_single_instance::init(
            move |app, _args, _cwd| {
                log::info!("Reopening the app, single instance handler");
                log::debug!("app {app:?}");
                #[cfg(target_os = "macos")]
                {
                    let location_set = location_set_clone.lock().unwrap();
                    if !*location_set {
                        log::info!("Location not set, don't show the main window");
                        return;
                    }

                    let main_window = app.get_webview_window("main");
                    if let Some(window) = main_window {
                        log::info!("Single instance handler: showing main window");
                        let _ = window.show();
                        // On macOS showing the panel is enough, and focusing
                        // would take the focus from the app in front.
                        #[cfg(not(target_os = "macos"))]
                        let _ = window.set_focus();
                    } else {
                        log::error!("Main window not found");
                    }
                }
            },
        ));
    }
    let log_file_name = if cfg!(debug_assertions) {
        Some("debug".to_string())
    } else {
        None
    };
    let app = app
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_http::init())
        .plugin(tauri_plugin_positioner::init())
        .plugin(
            tauri_plugin_log::Builder::default()
                .targets([
                    Target::new(TargetKind::LogDir {
                        file_name: log_file_name,
                    }),
                    Target::new(TargetKind::Stdout),
                    Target::new(TargetKind::Webview),
                ])
                .level(LevelFilter::Warn)
                .level_for("hopp", log_level)
                .level_for("sentry_utils", log_level)
                .max_file_size(50 * 1024 * 1024) // We are emptying them on startup
                .build(),
        )
        .setup(move |app| {
            /* Create the app_data_dir if it doesn't exist. */
            let app_data_dir = app
                .path()
                .app_data_dir()
                .expect("Failed to get app data dir.");
            if !app_data_dir.exists() {
                if let Err(e) = std::fs::create_dir_all(&app_data_dir) {
                    log::error!("Failed to create app data dir: {e:?}");
                }
            }
            hopp::migrate_state_from_official_app(&app_data_dir);

            let (_core_process, sender, mut event_socket) =
                create_core_process(app.handle()).expect("Failed to create core process");

            let core_events_rx = event_socket.take_events();

            let app_state = AppState::new(&app_data_dir);
            let noise_cancellation_enabled = app_state.user_settings().noise_cancellation_enabled;
            if let Err(e) = sender.send(Message::SetNoiseCancellation(noise_cancellation_enabled)) {
                log::error!("Failed to send initial noise_cancellation_enabled: {e:?}");
            }
            let transcription = hopp::transcription::core_settings(&app_data_dir, &app_state.user_settings());
            if let Err(e) = sender.send(Message::SetTranscription(transcription)) {
                log::error!("Failed to send initial transcription settings: {e:?}");
            }
            let share_system_audio = app_state.user_settings().share_system_audio;
            if let Err(e) = sender.send(Message::SetShareSystemAudio(share_system_audio)) {
                log::error!("Failed to send initial share_system_audio: {e:?}");
            }
            let webcam_preview = app_state.user_settings().webcam_preview;
            if let Err(e) = sender.send(Message::SetWebcamPreview(webcam_preview)) {
                log::error!("Failed to send initial webcam_preview: {e:?}");
            }
            let auto_mic_volume = app_state.user_settings().auto_mic_volume;
            if let Err(e) = sender.send(Message::SetAutoMicVolume(auto_mic_volume)) {
                log::error!("Failed to send initial auto_mic_volume: {e:?}");
            }
            let audio_output = app_state.user_settings().audio_output;
            if let Err(e) = sender.send(Message::SetAudioOutput(audio_output)) {
                log::error!("Failed to send initial audio_output: {e:?}");
            }
            let screen_share_resolution = app_state.user_settings().screen_share_resolution;
            if let Err(e) = sender.send(Message::SetScreenShareResolution(screen_share_resolution)) {
                log::error!("Failed to send initial screen_share_resolution: {e:?}");
            }
            let screen_share_picker_mode = app_state.user_settings().screen_share_picker_mode;
            if let Err(e) = sender.send(Message::SetScreenSharePickerMode(screen_share_picker_mode)) {
                log::error!("Failed to send initial screen_share_picker_mode: {e:?}");
            }
            let app_veil_bundle_ids = app_state
                .user_settings()
                .app_veil_applications
                .into_iter()
                .filter(|application| application.enabled)
                .map(|application| application.bundle_id)
                .collect();
            if let Err(e) = sender.send(Message::SetAppVeilBundleIds(app_veil_bundle_ids)) {
                log::error!("Failed to send initial App Veil settings: {e:?}");
            }
            if let Err(e) = sender.send(Message::ControllerDrawPersistChanged(app_state.controller_draw_persist())) {
                log::error!("Failed to send initial controller_draw_persist: {e:?}");
            }
            if let Some(mode) = app_state.last_mode() {
                if let Err(e) = sender.send(Message::LastModeChanged(mode)) {
                    log::error!("Failed to send initial last_mode: {e:?}");
                }
            }
            {
                let telemetry_enabled = app_state.user_settings().telemetry_enabled;
                sentry_utils::set_telemetry_enabled(telemetry_enabled);
                if let Err(e) = sender.send(Message::SetTelemetryEnabled(telemetry_enabled)) {
                    log::error!("Failed to send initial telemetry_enabled: {e:?}");
                }
            }
            let data = Mutex::new(AppData::new(
                sender,
                event_socket,
                deactivate_hiding_clone,
                app_state,
                suppress_hide_on_call_end.clone(),
            ));
            app.manage(data);

            // Background thread to forward core events to the frontend
            let event_app_handle = app.handle().clone();
            std::thread::spawn(move || {
                forward_core_events(core_events_rx, event_app_handle);
            });

            std::thread::spawn(|| {
                sentry_utils::upload_latest_crash();
            });

            let quit = MenuItemBuilder::new("Quit").id("quit").build(app)?;
            let menu = MenuBuilder::new(app).items(&[&quit]).build()?;

            setup_tray_icon(app, &menu, location_set_setup.clone(), tray_clicked.clone())?;

            #[cfg(target_os = "macos")]
            {
                app.set_menu(build_app_menu(app.handle())?)?;
                app.on_menu_event(|app, event| {
                    if event.id.as_ref() == CLOSE_WINDOW_MENU_ID {
                        close_focused_window(app);
                    }
                });
            }

            /* Clear app logs in the beginning of a session. */
            let dir = app.path().app_log_dir();
            if let Err(e) = dir {
                log::warn!("Failed to get app log dir: {e:?}");
            } else {
                let dir = dir.unwrap();
                let log_file = dir.join("hopp.log");
                if log_file.exists() {
                    if let Err(e) = std::fs::write(&log_file, "") {
                        log::warn!("Failed to clear log file: {e:?}");
                    }
                }
            }

            /*
             * We are sending a ping event to the frontend
             * to keep it alive.
             * TODO: do graceful shutdown on exit
             */
            let app_handle = app.handle().clone();
            std::thread::spawn(move || {
                ping_frontend(app_handle);
            });

            let first_run = {
                let data = app.state::<Mutex<AppData>>();
                let data = data.lock().unwrap();
                data.app_state.first_run()
            };

            setup_start_on_launch(&app.autolaunch(), first_run);

            /* Set first run to false after checking the start on launch. */
            {
                let data = app.state::<Mutex<AppData>>();
                let mut data = data.lock().unwrap();
                if first_run {
                    data.app_state.set_first_run(false);
                }
            }

            /* Main window configuration on windows */
            #[cfg(any(target_os = "windows", target_os = "linux"))]
            {
                let handle = app.handle();
                if let Some(window) = handle.get_webview_window("main") {
                    let _ = window.set_shadow(false);
                    let _ = window.set_skip_taskbar(false);
                    /* Place window on the bottom right corner of the active display. */
                    let current_monitor = window.current_monitor();
                    if let Ok(Some(monitor)) = current_monitor {
                        let monitor_size = monitor.size();
                        let monitor_pos = monitor.position();
                        let window_size = window.inner_size().unwrap();
                        let base_offset = 20 * monitor.scale_factor() as u32;
                        let offset_y = (25. * monitor.scale_factor()) as u32 + base_offset;
                        let x = monitor_pos.x
                            + (monitor_size.width - window_size.width - base_offset) as i32;
                        let y = monitor_pos.y
                            + (monitor_size.height - window_size.height - offset_y) as i32;
                        let new_position = PhysicalPosition::new(x as f64, y as f64);
                        let _ = window.set_position(new_position);
                    }
                    let _ = window.set_always_on_top(false);
                    let _ = window.show();
                }
            }

            /* macOS specific setup */
            #[cfg(target_os = "macos")]
            {
                disable_app_nap();
                /* Start as Accessory — switch to Regular during calls or when permission windows are visible */
                app.set_activation_policy(tauri::ActivationPolicy::Accessory);

                /*
                 * First show the notification window which explains that hopp lives in the
                 * menubar. Then show the permissions window if needed.
                 */
                let mut show_dock = false;
                let show_tray_notification_selection = {
                    let data = app.state::<Mutex<AppData>>();
                    let data = data.lock().unwrap();
                    data.app_state.tray_notification()
                };
                if show_tray_notification_selection {
                    let height = 250.;
                    let width = 450.;

                    let notification_window = tauri::WebviewWindowBuilder::new(
                        app,
                        "trayNotification",
                        tauri::WebviewUrl::App("trayNotification.html".into()),
                    )
                    .visible(true)
                    .focused(true)
                    .resizable(false)
                    .hidden_title(true)
                    .always_on_top(true)
                    .title_bar_style(tauri::TitleBarStyle::Overlay)
                    .title("Tray Notification")
                    .inner_size(width, height)
                    .build();
                    if let Err(e) = notification_window {
                        log::error!("Failed to create notification window: {e:?}");
                    } else {
                        let notification_window = notification_window.unwrap();
                        let _ = notification_window.show();
                        let _ = notification_window.set_focus();
                        show_dock = true;
                    }
                }

                if permissions::has_ungranted_permissions() {
                    log::info!("Opening permissions window");
                    let permissions_window = tauri::WebviewWindowBuilder::new(
                        app,
                        "permissions",
                        tauri::WebviewUrl::App("permissions.html".into()),
                    )
                    .visible(false)
                    .focused(true)
                    .resizable(false)
                    .hidden_title(true)
                    .always_on_top(false)
                    .title_bar_style(tauri::TitleBarStyle::Overlay)
                    .title("Permissions Configuration")
                    .inner_size(900., 730.)
                    .transparent(true)
                    .shadow(true)
                    .build();
                    if let Err(e) = permissions_window {
                        log::error!("Failed to create permissions window: {e:?}");
                    } else {
                        let permissions_window = permissions_window.unwrap();
                        show_dock = true;

                        // Apply native styling on macOS
                        #[cfg(target_os = "macos")]
                        {
                            set_window_corner_radius_and_decorations(
                                &permissions_window,
                                CORNER_RADIUS,
                                true,
                            );
                        }

                        /*
                         * Focus the window only if the notification window is not shown.
                         * When the notification window is shown we open the permissions window
                         * when it's closed.
                         */
                        if !show_tray_notification_selection {
                            let _ = permissions_window.show();
                            let _ = permissions_window.set_focus();
                        }
                    }
                }

                // Tackles Alt+Tab activation
                if show_dock {
                    app.set_activation_policy(tauri::ActivationPolicy::Regular);
                }
                {
                    let data = app.state::<Mutex<AppData>>();
                    let mut data = data.lock().unwrap();
                    if show_dock {
                        data.activation_policy_regular = true;
                    }
                    if !cfg!(debug_assertions) {
                        data.activation_observer =
                            Some(hopp::app_activation::AppActivationObserver::new(
                                app.handle().clone(),
                                location_set_setup.clone(),
                                reopen_requested_clone.clone(),
                                tray_clicked.clone(),
                            ));
                    }
                }
            }

            Ok(())
        })
        .on_window_event(move |window, event| {
            if let tauri::WindowEvent::Focused(is_focused) = event {
                #[cfg(any(target_os = "windows", target_os = "linux"))]
                if *is_focused && window.label() == "main" {
                    /* Place window on the bottom right corner of the active display. */
                    let current_monitor = window.current_monitor();
                    if let Ok(Some(monitor)) = current_monitor {
                        let monitor_size = monitor.size();
                        let monitor_pos = monitor.position();
                        let window_size = window.inner_size().unwrap();
                        let base_offset = 20 * monitor.scale_factor() as u32;
                        let offset_y = (25. * monitor.scale_factor()) as u32 + base_offset;
                        let x = monitor_pos.x
                            + (monitor_size.width - window_size.width - base_offset) as i32;
                        let y = monitor_pos.y
                            + (monitor_size.height - window_size.height - offset_y) as i32;
                        let new_position = PhysicalPosition::new(x as f64, y as f64);
                        let _ = window.set_position(new_position);
                    }
                }

                // detect click outside of the focused window and hide the app
                let deactivate_hiding = deactivate_hiding.lock().unwrap();
                let reopen_requested = reopen_requested.lock().unwrap();
                if !is_focused
                    && window.label() == "main"
                    && !cfg!(debug_assertions)
                    && !*deactivate_hiding
                    && !*reopen_requested
                    && !suppress_hide_on_call_end_clone.load(Ordering::Relaxed)
                {
                    log::info!("Hiding main window on focus lost: {}", *reopen_requested);

                    #[cfg(target_os = "macos")]
                    {
                        window.hide().unwrap();
                        hopp::note_main_window_hidden_on_blur();
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            stop_sharing,
            get_available_content,
            store_token_cmd,
            get_stored_token,
            delete_stored_token,
            play_sound,
            stop_sound,
            reset_core_process,
            get_logs,
            set_deactivate_hiding,
            set_controller_cursor,
            open_accessibility_settings,
            open_microphone_settings,
            open_screenshare_settings,
            trigger_screenshare_permission,
            get_control_permission,
            get_microphone_permission,
            get_screenshare_permission,
            skip_tray_notification_selection_window,
            set_last_used_mic,
            get_last_used_mic,
            set_last_used_camera,
            get_last_used_camera,
            get_sharer_draw_persist,
            set_draw_persist,
            get_drawing_hint_shown,
            set_drawing_hint_shown,
            get_drawing_enabled,
            set_drawing_enabled,
            minimize_main_window,
            hide_main_window,
            set_livekit_url,
            get_livekit_url,
            get_camera_permission,
            open_camera_settings,
            set_sentry_metadata,
            call_started,
            set_tray_notification,
            get_hopp_server_url,
            set_hopp_server_url,
            create_feedback_window,
            create_settings_window,
            get_user_settings,
            set_default_mic,
            get_preferred_microphone,
            list_installed_applications,
            set_app_veil_applications,
            set_call_feedback_popup,
            set_telemetry_enabled,
            set_show_dock_icon_in_call,
            set_notify_when_alone,
            set_auto_update_enabled,
            set_start_camera_on_call,
            set_start_mic_on_call,
            set_remote_control_enabled,
            set_shortcut_toggle_mic,
            set_shortcut_toggle_camera,
            set_shortcut_toggle_screenshare,
            set_shortcut_end_call,
            set_is_camera_on,
            set_is_screensharing,
            mute_mic,
            unmute_mic,
            toggle_mic,
            set_noise_cancellation,
            set_screen_share_resolution,
            set_screen_share_picker_mode,
            list_microphones,
            list_speakers,
            set_audio_output,
            set_auto_mic_volume,
            set_webcam_preview,
            set_open_received_links,
            set_share_system_audio,
            get_transcription_info,
            download_transcription_model,
            set_transcription_enabled,
            set_transcription_language,
            open_transcripts_folder,
            select_microphone,
            list_webcams,
            start_camera,
            stop_camera,
            open_camera_preview,
            open_screenshare_viewer,
            close_screenshare_viewer,
            end_call,
            toggle_call_sleep_prevention,
            bring_windows_to_front,
            open_stats_window,
            quit_app,
        ])
        .build(tauri::generate_context!())
        .expect("error while running tauri application");

    app.run(move |app_handle, event| match event {
        tauri::RunEvent::ExitRequested { .. } => {
            log::info!("Exit requested");
            sentry_utils::upload_logs_event("Tauri app quit".to_string());
            sentry_utils::flush(std::time::Duration::from_secs(2));
        }
        #[cfg(target_os = "macos")]
        tauri::RunEvent::WindowEvent {
            label,
            event: tauri::WindowEvent::Destroyed,
            ..
        } if label == "settings" => {
            app_handle
                .state::<Mutex<AppData>>()
                .lock()
                .unwrap()
                .settings_window_open = false;
            sync_dock_icon(app_handle);
        }
        tauri::RunEvent::WindowEvent {
            label,
            event: tauri::WindowEvent::CloseRequested { .. },
            ..
        } => {
            log::info!("Close requested for window: {label}");
            if label == "trayNotification" {
                /* Make the permissions window visible in this case. */
                let permissions_window = app_handle.get_webview_window("permissions");
                if let Some(window) = permissions_window {
                    log::info!("Show permissions window");
                    let _ = window.show();
                    let _ = window.set_focus();
                } else {
                    #[cfg(target_os = "macos")]
                    {
                        let _ =
                            app_handle.set_activation_policy(tauri::ActivationPolicy::Accessory);
                        app_handle
                            .state::<Mutex<AppData>>()
                            .lock()
                            .unwrap()
                            .activation_policy_regular = false;
                    }
                }
            } else if label == "permissions" {
                #[cfg(target_os = "macos")]
                {
                    let _ = app_handle.set_activation_policy(tauri::ActivationPolicy::Accessory);
                    app_handle
                        .state::<Mutex<AppData>>()
                        .lock()
                        .unwrap()
                        .activation_policy_regular = false;
                }
            }
        }
        _ => {}
    });
}
