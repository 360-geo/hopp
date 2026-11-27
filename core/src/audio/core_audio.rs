//! Small CoreAudio helpers for audio devices by name, as cpal reports them:
//! finding a device, reading and setting its properties, and telling whether
//! an output is headphones.

/// Whether the output named `output_device` (the system default output when
/// `None`) is headphones: Bluetooth (AirPods and the like), the headphone
/// jack, or a USB device that calls itself a headset or headphones.
#[cfg(target_os = "macos")]
pub fn output_is_headphones(output_device: Option<&str>) -> bool {
    macos::output_is_headphones(output_device)
}

#[cfg(not(target_os = "macos"))]
pub fn output_is_headphones(_output_device: Option<&str>) -> bool {
    false
}

#[cfg(target_os = "macos")]
pub(crate) mod macos {
    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};
    use objc2_core_audio::{
        kAudioDevicePropertyDataSource, kAudioDevicePropertyStreams,
        kAudioDevicePropertyTransportType, kAudioDeviceTransportTypeBluetooth,
        kAudioDeviceTransportTypeBluetoothLE, kAudioDeviceTransportTypeBuiltIn,
        kAudioDeviceTransportTypeUSB, kAudioHardwarePropertyDefaultInputDevice,
        kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyDevices,
        kAudioObjectPropertyElementMain, kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyScopeInput, kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject,
        AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
        AudioObjectPropertyAddress, AudioObjectSetPropertyData,
    };
    use std::ffi::c_void;
    use std::ptr::{self, NonNull};

    /// The built-in output's data source when headphones are plugged in.
    const DATA_SOURCE_HEADPHONES: u32 = u32::from_be_bytes(*b"hdpn");

    #[derive(Clone, Copy)]
    pub enum Direction {
        Input,
        Output,
    }

    impl Direction {
        pub fn scope(self) -> u32 {
            match self {
                Direction::Input => kAudioObjectPropertyScopeInput,
                Direction::Output => kAudioObjectPropertyScopeOutput,
            }
        }
    }

    pub fn address(selector: u32, scope: u32, element: u32) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: scope,
            mElement: element,
        }
    }

    fn global(selector: u32) -> AudioObjectPropertyAddress {
        address(
            selector,
            kAudioObjectPropertyScopeGlobal,
            kAudioObjectPropertyElementMain,
        )
    }

    /// Reads a fixed-size property. `value` goes in as the input of
    /// translation properties and comes back as the result.
    pub fn read<T: Copy>(
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

    pub fn write_f32(
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

    fn default_device(direction: Direction) -> Option<AudioObjectID> {
        let selector = match direction {
            Direction::Input => kAudioHardwarePropertyDefaultInputDevice,
            Direction::Output => kAudioHardwarePropertyDefaultOutputDevice,
        };
        read(
            kAudioObjectSystemObject as AudioObjectID,
            global(selector),
            0u32,
        )
        .filter(|&id| id != 0)
    }

    fn all_devices() -> Vec<AudioObjectID> {
        let address = global(kAudioHardwarePropertyDevices);
        let system = kAudioObjectSystemObject as AudioObjectID;
        let count = data_size(system, address) as usize / std::mem::size_of::<AudioObjectID>();
        if count == 0 {
            return vec![];
        }
        let mut devices = vec![0 as AudioObjectID; count];
        let mut size = std::mem::size_of_val(devices.as_slice()) as u32;
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

    fn has_streams(device: AudioObjectID, direction: Direction) -> bool {
        let streams = address(
            kAudioDevicePropertyStreams,
            direction.scope(),
            kAudioObjectPropertyElementMain,
        );
        data_size(device, streams) > 0
    }

    pub fn device_name(device: AudioObjectID) -> Option<String> {
        let raw: CFStringRef = read(device, global(kAudioObjectPropertyName), ptr::null())?;
        if raw.is_null() {
            return None;
        }
        // The caller owns the returned string.
        Some(unsafe { CFString::wrap_under_create_rule(raw) }.to_string())
    }

    /// The device named `name` that has `direction` streams, or the system
    /// default for `direction` when `name` is `None`.
    pub fn find_device(name: Option<&str>, direction: Direction) -> Option<AudioObjectID> {
        match name {
            None => default_device(direction),
            Some(name) => all_devices().into_iter().find(|&device| {
                has_streams(device, direction) && device_name(device).as_deref() == Some(name)
            }),
        }
    }

    pub fn output_is_headphones(output_device: Option<&str>) -> bool {
        // A picked output that's gone falls back to the default, as playback does.
        let Some(device) = find_device(output_device, Direction::Output)
            .or_else(|| default_device(Direction::Output))
        else {
            return false;
        };
        let transport = read(device, global(kAudioDevicePropertyTransportType), 0u32);
        match transport {
            Some(t)
                if t == kAudioDeviceTransportTypeBluetooth
                    || t == kAudioDeviceTransportTypeBluetoothLE =>
            {
                true
            }
            Some(t) if t == kAudioDeviceTransportTypeBuiltIn => {
                let source = address(
                    kAudioDevicePropertyDataSource,
                    kAudioObjectPropertyScopeOutput,
                    kAudioObjectPropertyElementMain,
                );
                read(device, source, 0u32) == Some(DATA_SOURCE_HEADPHONES)
            }
            Some(t) if t == kAudioDeviceTransportTypeUSB => {
                device_name(device).is_some_and(|name| {
                    let name = name.to_lowercase();
                    ["headset", "headphone", "earbud", "airpods"]
                        .iter()
                        .any(|word| name.contains(word))
                })
            }
            _ => false,
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    /// Needs a Mac; reads the current default output without changing anything.
    #[test]
    #[ignore]
    fn reads_the_default_output_route() {
        let device = super::macos::find_device(None, super::macos::Direction::Output);
        let name = device.and_then(super::macos::device_name);
        println!(
            "default output {name:?}, headphones: {}",
            super::output_is_headphones(None)
        );
        assert!(name.is_some());
    }
}
