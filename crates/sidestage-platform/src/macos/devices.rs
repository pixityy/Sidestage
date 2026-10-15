//! Output devices: listing, resolving the configured one, and the facts the engine needs.

use anyhow::{Context, bail};
use objc2_core_audio::{
    AudioObjectID, kAudioDevicePropertyBufferFrameSize, kAudioDevicePropertyDeviceIsAlive,
    kAudioDevicePropertyDeviceUID, kAudioDevicePropertyIsHidden, kAudioDevicePropertyNominalSampleRate,
    kAudioDevicePropertyPreferredChannelsForStereo, kAudioDevicePropertySafetyOffset,
    kAudioDevicePropertyStreamConfiguration, kAudioDevicePropertyStreams, kAudioDevicePropertyTransportType,
    kAudioDeviceTransportTypeAggregate, kAudioDeviceTransportTypeBluetooth, kAudioDeviceTransportTypeBluetoothLE,
    kAudioDeviceTransportTypeUnknown, kAudioDeviceTransportTypeVirtual, kAudioHardwarePropertyDefaultOutputDevice,
    kAudioHardwarePropertyDevices, kAudioHardwarePropertyTranslateUIDToDevice, kAudioObjectPropertyName,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeInput, kAudioObjectPropertyScopeOutput,
    kAudioObjectUnknown, kAudioStreamPropertyVirtualFormat,
};
use objc2_core_foundation::CFString;

use super::ca::{self, SYSTEM_OBJECT};
use super::plan::AGGREGATE_UID_PREFIX;
use crate::OutputDevice;

const GLOBAL: u32 = kAudioObjectPropertyScopeGlobal;
const OUTPUT: u32 = kAudioObjectPropertyScopeOutput;
const INPUT: u32 = kAudioObjectPropertyScopeInput;

/// The device Sidestage plays to.
#[derive(Clone, Debug)]
pub struct Output {
    pub id: AudioObjectID,
    pub uid: String,
    pub name: String,
    pub transport: u32,
}

impl Output {
    pub fn load(id: AudioObjectID) -> anyhow::Result<Self> {
        Ok(Self {
            id,
            uid: device_uid(id)?,
            name: device_name(id),
            transport: ca::get_u32(id, kAudioDevicePropertyTransportType, GLOBAL)
                .unwrap_or(kAudioDeviceTransportTypeUnknown),
        })
    }

    /// Bluetooth (and virtual or unidentified) outputs run taps and output on one clock, so
    /// tap drift compensation only adds periodic sample slips there (FineTune's finding).
    pub fn shares_tap_clock(&self) -> bool {
        [
            kAudioDeviceTransportTypeBluetooth,
            kAudioDeviceTransportTypeBluetoothLE,
            kAudioDeviceTransportTypeVirtual,
            kAudioDeviceTransportTypeUnknown,
        ]
        .contains(&self.transport)
    }

    /// A user-made aggregate or multi-output device, which cannot be the clock of Sidestage's
    /// own aggregate (Core Audio does not nest aggregates).
    pub fn is_aggregate(&self) -> bool {
        self.transport == kAudioDeviceTransportTypeAggregate
    }
}

pub fn default_output() -> anyhow::Result<AudioObjectID> {
    let id = ca::get_u32(SYSTEM_OBJECT, kAudioHardwarePropertyDefaultOutputDevice, GLOBAL)
        .context("reading the default output device")?;
    if id == kAudioObjectUnknown {
        bail!("macOS reports no default output device");
    }
    Ok(id)
}

pub fn device_uid(id: AudioObjectID) -> anyhow::Result<String> {
    ca::get_string(id, kAudioDevicePropertyDeviceUID, GLOBAL)?.with_context(|| format!("audio device {id} has no UID"))
}

pub fn device_name(id: AudioObjectID) -> String {
    ca::get_string(id, kAudioObjectPropertyName, GLOBAL)
        .ok()
        .flatten()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| format!("Audio device {id}"))
}

/// The device with this UID, if it is connected.
pub fn device_for_uid(uid: &str) -> Option<AudioObjectID> {
    let uid = CFString::from_str(uid);
    let qualifier: *const CFString = &*uid;
    // SAFETY: the qualifier is a CFStringRef, the value an AudioObjectID.
    let id: AudioObjectID = unsafe {
        ca::get_qualified(SYSTEM_OBJECT, kAudioHardwarePropertyTranslateUIDToDevice, GLOBAL, Some(&qualifier))
    }
    .ok()?;
    (id != kAudioObjectUnknown).then_some(id)
}

pub fn stream_count(id: AudioObjectID, scope: u32) -> anyhow::Result<usize> {
    Ok(ca::get_ids(id, kAudioDevicePropertyStreams, scope)?.len())
}

pub fn input_stream_count(id: AudioObjectID) -> anyhow::Result<usize> {
    stream_count(id, INPUT).context("counting input streams")
}

pub fn has_output(id: AudioObjectID) -> bool {
    stream_count(id, OUTPUT).is_ok_and(|n| n > 0)
}

/// Channels of each output stream, in stream order.
pub fn output_channels(id: AudioObjectID) -> anyhow::Result<Vec<usize>> {
    ca::stream_channels(id, kAudioDevicePropertyStreamConfiguration, OUTPUT)
        .context("reading the output channel layout")
}

/// Fails unless every output stream carries 32-bit float samples, the only format the IOProc
/// writes. (The HAL converts for its clients, so this only fails for unusual devices.)
pub fn check_output_formats(id: AudioObjectID) -> anyhow::Result<()> {
    for stream in ca::get_ids(id, kAudioDevicePropertyStreams, OUTPUT).context("listing output streams")? {
        let format = ca::get_asbd(stream, kAudioStreamPropertyVirtualFormat, GLOBAL)
            .context("reading an output stream format")?;
        if !ca::is_float32(&format) {
            bail!(
                "the output device uses an unsupported sample format ({} bits, flags {:#x})",
                format.mBitsPerChannel,
                format.mFormatFlags
            );
        }
    }
    Ok(())
}

pub fn preferred_stereo(id: AudioObjectID) -> Option<[u32; 2]> {
    // SAFETY: two UInt32 channel numbers.
    unsafe { ca::get(id, kAudioDevicePropertyPreferredChannelsForStereo, OUTPUT) }.ok()
}

pub fn nominal_rate(id: AudioObjectID) -> anyhow::Result<f64> {
    ca::get_f64(id, kAudioDevicePropertyNominalSampleRate, GLOBAL).context("reading the sample rate")
}

pub fn set_nominal_rate(id: AudioObjectID, rate: f64) -> anyhow::Result<()> {
    // SAFETY: the property is a Float64.
    unsafe { ca::set(id, kAudioDevicePropertyNominalSampleRate, GLOBAL, &rate) }
}

pub fn is_alive(id: AudioObjectID) -> bool {
    ca::get_u32(id, kAudioDevicePropertyDeviceIsAlive, GLOBAL).is_ok_and(|v| v != 0)
}

pub fn buffer_frames(id: AudioObjectID) -> u32 {
    ca::get_u32(id, kAudioDevicePropertyBufferFrameSize, GLOBAL).unwrap_or(0)
}

pub fn safety_offset(id: AudioObjectID, input: bool) -> u32 {
    ca::get_u32(id, kAudioDevicePropertySafetyOffset, if input { INPUT } else { OUTPUT }).unwrap_or(0)
}

/// Output devices for the picker. Hidden devices and Sidestage's own private aggregates are
/// left out.
pub fn list_outputs() -> anyhow::Result<Vec<OutputDevice>> {
    let default = default_output().ok();
    let mut out = Vec::new();
    for id in ca::get_ids(SYSTEM_OBJECT, kAudioHardwarePropertyDevices, GLOBAL).context("listing audio devices")? {
        if !has_output(id) || ca::get_u32(id, kAudioDevicePropertyIsHidden, GLOBAL).is_ok_and(|h| h != 0) {
            continue;
        }
        let Ok(uid) = device_uid(id) else { continue };
        if uid.starts_with(AGGREGATE_UID_PREFIX) {
            continue;
        }
        out.push(OutputDevice { id: uid, name: device_name(id), is_default: default == Some(id) });
    }
    Ok(out)
}

/// The output to play to: the configured device, or the system default when none is
/// configured or the configured one is not connected (then with a note for the user).
pub fn resolve(wanted: Option<&str>) -> anyhow::Result<(Output, Option<String>)> {
    if let Some(uid) = wanted {
        if let Some(id) = device_for_uid(uid) {
            return Ok((Output::load(id)?, None));
        }
        let fallback = Output::load(default_output()?)?;
        let note = format!("The chosen output device is not connected; playing to {} instead.", fallback.name);
        return Ok((fallback, Some(note)));
    }
    Ok((Output::load(default_output()?)?, None))
}
