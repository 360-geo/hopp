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
    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};
    use objc2_core_audio::{
        kAudioDevicePropertyStreams, kAudioDevicePropertyVolumeDecibelsToScalar,
        kAudioDevicePropertyVolumeScalar, kAudioDevicePropertyVolumeScalarToDecibels,
        kAudioHardwarePropertyDefaultInputDevice, kAudioHardwarePropertyDevices,
        kAudioObjectPropertyElementMain, kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyScopeInput, kAudioObjectSystemObject, AudioObjectGetPropertyData,
        AudioObjectGetPropertyDataSize, AudioObjectHasProperty, AudioObjectID,
        AudioObjectIsPropertySettable, AudioObjectPropertyAddress, AudioObjectSetPropertyData,
    };
    use std::ffi::c_void;
    use std::ptr::{self, NonNull};

    /// Automatic adjustment never turns a microphone down below this.
    const MIN_VOLUME: f32 = 0.1;
    /// Without a dB mapping from the device, this many dB move the slider by 1.0.
    const FALLBACK_DB_PER_FULL_RANGE: f32 = 40.0;

    fn address(selector: u32, scope: u32, element: u32) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: scope,
            mElement: element,
        }
    }

    /// Reads a fixed-size property. `value` goes in as the qualifier-free
    /// input for translation properties and comes back as the result.
    fn read<T: Copy>(
        object: AudioObjectID,
        address: AudioObjectPropertyAddress,
        mut value: T,
    ) -> Option<T> {
        let mut size = std::mem::size_of::<T>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                NonNull::from(&address),
                0,
                ptr::null(),
                NonNull::from(&mut size),
                NonNull::from(&mut value).cast::<c_void>(),
            )
        };
        (status == 0).then_some(value)
    }

    fn write_f32(
        object: AudioObjectID,
        address: AudioObjectPropertyAddress,
        mut value: f32,
    ) -> bool {
        let status = unsafe {
            AudioObjectSetPropertyData(
                object,
                NonNull::from(&address),
                0,
                ptr::null(),
                std::mem::size_of::<f32>() as u32,
                NonNull::from(&mut value).cast::<c_void>(),
            )
        };
        status == 0
    }

    fn data_size(object: AudioObjectID, address: AudioObjectPropertyAddress) -> u32 {
        let mut size = 0u32;
        let status = unsafe {
            AudioObjectGetPropertyDataSize(
                object,
                NonNull::from(&address),
                0,
                ptr::null(),
                NonNull::from(&mut size),
            )
        };
        if status == 0 {
            size
        } else {
            0
        }
    }

    fn global(selector: u32) -> AudioObjectPropertyAddress {
        address(
            selector,
            kAudioObjectPropertyScopeGlobal,
            kAudioObjectPropertyElementMain,
        )
    }

    fn default_input_device() -> Option<AudioObjectID> {
        read(
            kAudioObjectSystemObject as AudioObjectID,
            global(kAudioHardwarePropertyDefaultInputDevice),
            0u32,
        )
        .filter(|&id| id != 0)
    }

    fn all_devices() -> Vec<AudioObjectID> {
        let address = global(kAudioHardwarePropertyDevices);
        let system = kAudioObjectSystemObject as AudioObjectID;
        let count = data_size(system, address) as usize / std::mem::size_of::<AudioObjectID>();
        let mut devices = vec![0 as AudioObjectID; count];
        let mut size = std::mem::size_of_val(devices.as_slice()) as u32;
        if count == 0 {
            return devices;
        }
        let status = unsafe {
            AudioObjectGetPropertyData(
                system,
                NonNull::from(&address),
                0,
                ptr::null(),
                NonNull::from(&mut size),
                NonNull::new(devices.as_mut_ptr().cast::<c_void>()).expect("vector pointer"),
            )
        };
        if status != 0 {
            return vec![];
        }
        devices.truncate(size as usize / std::mem::size_of::<AudioObjectID>());
        devices
    }

    fn has_input(device: AudioObjectID) -> bool {
        let streams = address(
            kAudioDevicePropertyStreams,
            kAudioObjectPropertyScopeInput,
            kAudioObjectPropertyElementMain,
        );
        data_size(device, streams) > 0
    }

    fn device_name(device: AudioObjectID) -> Option<String> {
        let raw: CFStringRef = read(device, global(kAudioObjectPropertyName), ptr::null())?;
        if raw.is_null() {
            return None;
        }
        // The caller owns the returned string.
        Some(unsafe { CFString::wrap_under_create_rule(raw) }.to_string())
    }

    fn find_input_device(name: Option<&str>) -> Option<AudioObjectID> {
        match name {
            None => default_input_device(),
            Some(name) => all_devices()
                .into_iter()
                .find(|&device| has_input(device) && device_name(device).as_deref() == Some(name)),
        }
    }

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
        let device = find_input_device(device_name)
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
