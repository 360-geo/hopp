//! Reads and moves a microphone's macOS input volume: the slider in
//! System Settings > Sound > Input. Used by automatic microphone volume.

/// Moves the input volume of the microphone named `device_name` (the system
/// default input when `None`) by about `change_db`. Returns the volume before
/// and after, from 0.0 to 1.0, or `None` when it's already at its limit.
#[cfg(target_os = "macos")]
pub fn adjust_input_volume(
    device_name: Option<&str>,
    change_db: f32,
) -> Result<Option<(f32, f32)>, String> {
    macos::adjust_input_volume(device_name, change_db)
}

#[cfg(not(target_os = "macos"))]
pub fn adjust_input_volume(
    _device_name: Option<&str>,
    _change_db: f32,
) -> Result<Option<(f32, f32)>, String> {
    Err("Adjusting the input volume is only supported on macOS".to_string())
}

#[cfg(target_os = "macos")]
mod macos {
    use crate::audio::core_audio::macos::{address, find_device, read, write_f32, Direction};
    use objc2_core_audio::{
        kAudioDevicePropertyVolumeDecibelsToScalar, kAudioDevicePropertyVolumeScalar,
        kAudioDevicePropertyVolumeScalarToDecibels, kAudioObjectPropertyElementMain,
        kAudioObjectPropertyScopeInput, AudioObjectHasProperty, AudioObjectID,
        AudioObjectIsPropertySettable,
    };
    use std::ptr::NonNull;

    /// Automatic adjustment never turns a microphone down below this.
    const MIN_VOLUME: f32 = 0.1;
    /// Without a dB mapping from the device, this many dB move the slider by 1.0.
    const FALLBACK_DB_PER_FULL_RANGE: f32 = 40.0;

    /// The input volume controls to move: the device's main one, or else one per channel.
    fn volume_elements(device: AudioObjectID) -> Vec<u32> {
        let settable = |element: u32| {
            let volume = address(
                kAudioDevicePropertyVolumeScalar,
                kAudioObjectPropertyScopeInput,
                element,
            );
            if !unsafe { AudioObjectHasProperty(device, NonNull::from(&volume)) } {
                return false;
            }
            let mut is_settable: u8 = 0;
            let status = unsafe {
                AudioObjectIsPropertySettable(
                    device,
                    NonNull::from(&volume),
                    NonNull::from(&mut is_settable).cast(),
                )
            };
            status == 0 && is_settable != 0
        };
        if settable(kAudioObjectPropertyElementMain) {
            return vec![kAudioObjectPropertyElementMain];
        }
        (1..=2).filter(|&channel| settable(channel)).collect()
    }

    pub fn adjust_input_volume(
        device_name: Option<&str>,
        change_db: f32,
    ) -> Result<Option<(f32, f32)>, String> {
        let device = find_device(device_name, Direction::Input)
            .ok_or_else(|| format!("Input device {device_name:?} not found"))?;
        let elements = volume_elements(device);
        if elements.is_empty() {
            return Err(format!("{device_name:?} has no adjustable input volume"));
        }

        let mut changed = None;
        for element in elements {
            let volume_address = address(
                kAudioDevicePropertyVolumeScalar,
                kAudioObjectPropertyScopeInput,
                element,
            );
            let translate = |selector: u32, value: f32| {
                read(
                    device,
                    address(selector, kAudioObjectPropertyScopeInput, element),
                    value,
                )
            };
            let Some(volume) = read(device, volume_address, 0f32) else {
                continue;
            };
            let target = translate(kAudioDevicePropertyVolumeScalarToDecibels, volume)
                .and_then(|db| {
                    translate(kAudioDevicePropertyVolumeDecibelsToScalar, db + change_db)
                })
                .unwrap_or(volume + change_db / FALLBACK_DB_PER_FULL_RANGE);
            let target = target.clamp(MIN_VOLUME.min(volume), 1.0);
            if (target - volume).abs() < 0.005 {
                continue;
            }
            if write_f32(device, volume_address, target) {
                changed = Some((volume, target));
            }
        }
        Ok(changed)
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    /// Needs a Mac with a microphone; moves nothing (a 0 dB change).
    #[test]
    #[ignore]
    fn finds_the_default_input_volume() {
        assert_eq!(super::adjust_input_volume(None, 0.0), Ok(None));
    }
}
