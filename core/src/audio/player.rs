use super::mixer::{MixerHandle, SharedProcessor};
use thiserror::Error;
use winit::event_loop::EventLoopProxy;

use crate::UserEvent;

#[derive(Error, Debug)]
pub enum PlayerError {
    #[error("Failed to create mixer: {0}")]
    Mixer(String),
    #[error("Failed to start device monitor: {0}")]
    DeviceMonitor(String),
}

struct PlayerInner {
    mixer: MixerHandle,
    processor: SharedProcessor,
    _device_monitor: super::device_monitor::DeviceMonitor,
}

pub struct Player {
    inner: Option<PlayerInner>,
    event_loop_proxy: EventLoopProxy<UserEvent>,
    /// The output picked in Settings; `None` follows the system default.
    output_device: Option<String>,
}

impl Player {
    pub fn new(proxy: EventLoopProxy<UserEvent>) -> Self {
        Self {
            inner: None,
            event_loop_proxy: proxy,
            output_device: None,
        }
    }

    /// Plays call audio on `output_device` from now on, also mid-call.
    pub fn set_output_device(&mut self, output_device: Option<String>) {
        if self.output_device == output_device {
            return;
        }
        log::info!("Player::set_output_device({output_device:?})");
        self.output_device = output_device;
        self.reconnect();
    }

    /// The system default output changed. Playback moves along when it
    /// follows the default, or when the picked output has been disconnected.
    pub fn handle_default_output_changed(&self) {
        let picked_is_connected = self
            .output_device
            .as_deref()
            .is_some_and(super::mixer::output_device_exists);
        if !picked_is_connected {
            log::info!("Default audio output device changed, reconnecting...");
            self.reconnect();
        }
    }

    fn reconnect(&self) {
        if let Some(inner) = &self.inner {
            if let Err(e) = inner.mixer.reconnect(self.output_device.as_deref()) {
                log::error!("Failed to reconnect audio output: {e}");
            }
        }
    }

    #[allow(unused_variables)]
    pub fn start(&mut self) -> Result<(), PlayerError> {
        log::info!("Player::start");
        let (mixer, processor) =
            MixerHandle::new(self.output_device.as_deref()).map_err(PlayerError::Mixer)?;

        let device_monitor = super::device_monitor::DeviceMonitor::new(
            super::device_monitor::DeviceKind::Output,
            self.event_loop_proxy.clone(),
        )
        .map_err(|e| PlayerError::DeviceMonitor(e.to_string()))?;

        self.inner = Some(PlayerInner {
            mixer,
            processor,
            _device_monitor: device_monitor,
        });
        Ok(())
    }

    pub fn stop(&mut self) {
        log::info!("Player::stop");
        self.inner = None;
    }

    pub fn mixer(&self) -> Option<&MixerHandle> {
        self.inner.as_ref().map(|i| &i.mixer)
    }

    pub fn processor(&self) -> Option<SharedProcessor> {
        self.inner.as_ref().map(|i| i.processor.clone())
    }
}
