//! Which endpoint role a stream is following - and nothing else about it.
//!
//! Windows has a name, a description, and an interface name for every audio
//! endpoint. None of them are read here, and none may be written anywhere:
//! "Microphone (Realtek High Definition Audio)", "Headset Earphone (Jabra
//! Evolve2 65)" identify hardware in a particular person's house, and this
//! crate's output lands in a permanent database row.
//!
//! What IS worth recording is the role, which is a two-valued fact about
//! routing rather than about hardware.

use wasapi::{Device, DeviceEnumerator, Direction, Role};

use crate::Channel;
use crate::capture::CaptureError;

/// Which of Windows' two endpoint roles this stream is on.
///
/// Windows routes "communications" audio - Teams, Zoom, Discord, a soft phone -
/// separately from everything else, and a machine with a headset routinely has
/// a different default for each. The distinction matters to a loopback capture
/// because it says whether this stream is following the device a call would
/// actually use, which is the difference between recording a meeting and
/// recording the music that was playing on the speakers instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceCategory {
    /// The general-purpose default. Music, videos, system sounds.
    Console,
    /// Also the communications default, so calls route here too.
    Communications,
}

impl DeviceCategory {
    /// Stable code, persisted in `merge_meta.device_category`.
    pub const fn as_code(self) -> &'static str {
        match self {
            Self::Console => "console",
            Self::Communications => "communications",
        }
    }
}

/// Categorises an already-opened device by comparing endpoint IDs.
///
/// By ID rather than by name, which is the whole reason this is a function and
/// not a string comparison somewhere: the IDs are opaque and are never
/// retained, only compared. A machine where the console and communications
/// defaults are the same endpoint - the common case, one pair of speakers -
/// reports `Communications`, because that is the more specific true statement:
/// a call WOULD come out of this device.
///
/// Falls back to `Console` when the communications default cannot be read,
/// which is the conservative answer: it claims less.
pub(crate) fn categorize_device(
    enumerator: &DeviceEnumerator,
    direction: &Direction,
    device: &Device,
) -> DeviceCategory {
    let Ok(opened_id) = device.get_id() else {
        return DeviceCategory::Console;
    };
    let communications = enumerator
        .get_default_device_for_role(direction, &Role::Communications)
        .ok()
        .and_then(|endpoint| endpoint.get_id().ok());

    match communications {
        Some(id) if id == opened_id => DeviceCategory::Communications,
        _ => DeviceCategory::Console,
    }
}

/// Reports the role of the endpoint this channel would capture, without
/// opening a stream on it.
///
/// Answers which endpoint role a channel would follow without opening a
/// stream. Callers that need readiness proof must use [`crate::AudioCapture`]
/// so the configured stream format is actually initialized.
pub fn describe_default_endpoint(channel: Channel) -> Result<DeviceCategory, CaptureError> {
    let _ = wasapi::initialize_mta();
    let direction = match channel {
        Channel::Loopback => Direction::Render,
        Channel::Microphone => Direction::Capture,
    };
    let enumerator = DeviceEnumerator::new().map_err(|_| CaptureError::NoDevice)?;
    let device = enumerator
        .get_default_device(&direction)
        .map_err(|_| CaptureError::NoDevice)?;
    Ok(categorize_device(&enumerator, &direction, &device))
}

#[cfg(test)]
mod tests {
    use super::DeviceCategory;

    #[test]
    fn the_category_codes_are_the_two_windows_roles() {
        assert_eq!(DeviceCategory::Console.as_code(), "console");
        assert_eq!(DeviceCategory::Communications.as_code(), "communications");
    }
}
