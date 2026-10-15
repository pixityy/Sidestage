//! COM notification sinks and their registrations. Windows calls the sinks on its own MTA
//! threads; they only forward a message (to the session supervisor, the platform worker or the
//! guardian) and, for foreign volume changes, close the capture gate at once. They never call
//! back into the audio system.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::Sender;

use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    AudioSessionDisconnectReason, AudioSessionState, AudioSessionStateExpired, DEVICE_STATE, EDataFlow, ERole,
    IAudioSessionControl, IAudioSessionEvents, IAudioSessionEvents_Impl, IAudioSessionManager2,
    IAudioSessionNotification, IAudioSessionNotification_Impl, IAudioVolumeDuckNotification,
    IAudioVolumeDuckNotification_Impl, IMMDeviceEnumerator, IMMNotificationClient, IMMNotificationClient_Impl,
    eConsole, eRender,
};
use windows::core::{BOOL, GUID, PCWSTR, Ref, implement};

use super::capture::Gain;
use super::sessions::Manager;

/// Event context passed with every volume change Sidestage makes, so its own changes can be
/// told apart from the user's and the app's in `OnSimpleVolumeChanged`.
pub const SIDESTAGE_CONTEXT: GUID = GUID::from_u128(0x5d1e_57a6_e0b1_4c8a_9a6f_3b2d_7c41_e905);

/// Messages to the session supervisor, from notification sinks and from the session handle.
#[derive(Debug)]
pub enum Event {
    /// The owner wants the session stopped and restored.
    Stop,
    DefaultOutputChanged,
    DevicesChanged,
    SessionCreated,
    /// Someone other than Sidestage changed a hushed session's volume or mute.
    VolumeChanged {
        instance: Arc<str>,
        volume: f32,
        muted: bool,
    },
    SessionGone {
        instance: Arc<str>,
    },
    Ducking(bool),
}

#[implement(IMMNotificationClient)]
pub struct DeviceEvents(pub Sender<Event>);

impl IMMNotificationClient_Impl for DeviceEvents_Impl {
    fn OnDeviceStateChanged(&self, _id: &PCWSTR, _state: DEVICE_STATE) -> windows::core::Result<()> {
        let _ = self.0.send(Event::DevicesChanged);
        Ok(())
    }
    fn OnDeviceAdded(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        let _ = self.0.send(Event::DevicesChanged);
        Ok(())
    }
    fn OnDeviceRemoved(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        let _ = self.0.send(Event::DevicesChanged);
        Ok(())
    }
    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, _id: &PCWSTR) -> windows::core::Result<()> {
        if flow == eRender && role == eConsole {
            let _ = self.0.send(Event::DefaultOutputChanged);
        }
        Ok(())
    }
    fn OnPropertyValueChanged(&self, _id: &PCWSTR, _key: &PROPERTYKEY) -> windows::core::Result<()> {
        Ok(())
    }
}

/// Device notifications sent to a channel, registered with an enumerator until dropped.
pub struct DeviceWatch {
    en: IMMDeviceEnumerator,
    client: IMMNotificationClient,
}

impl DeviceWatch {
    pub fn register(en: &IMMDeviceEnumerator, tx: Sender<Event>) -> windows::core::Result<Self> {
        let client: IMMNotificationClient = DeviceEvents(tx).into();
        unsafe { en.RegisterEndpointNotificationCallback(&client) }?;
        Ok(DeviceWatch { en: en.clone(), client })
    }
}

impl Drop for DeviceWatch {
    fn drop(&mut self) {
        let _ = unsafe { self.en.UnregisterEndpointNotificationCallback(&self.client) };
    }
}

#[implement(IAudioSessionNotification)]
pub struct NewSessions(Box<dyn Fn() + Send + Sync>);

impl IAudioSessionNotification_Impl for NewSessions_Impl {
    fn OnSessionCreated(&self, _session: Ref<IAudioSessionControl>) -> windows::core::Result<()> {
        (self.0)();
        Ok(())
    }
}

/// New-session notifications of one endpoint, registered until dropped.
pub struct SessionWatch {
    mgr: IAudioSessionManager2,
    pub endpoint_id: String,
    sink: IAudioSessionNotification,
}

impl SessionWatch {
    /// Calls `on_created` (on a Windows thread) whenever a session is created on `m`'s
    /// endpoint. It must only signal, never call into the audio system.
    pub fn register(m: &Manager, on_created: impl Fn() + Send + Sync + 'static) -> windows::core::Result<Self> {
        // Notifications only start once the endpoint's sessions have been enumerated.
        if let Ok(en) = unsafe { m.mgr.GetSessionEnumerator() } {
            let _ = unsafe { en.GetCount() };
        }
        let sink: IAudioSessionNotification = NewSessions(Box::new(on_created)).into();
        unsafe { m.mgr.RegisterSessionNotification(&sink) }?;
        Ok(SessionWatch { mgr: m.mgr.clone(), endpoint_id: m.endpoint_id.clone(), sink })
    }
}

impl Drop for SessionWatch {
    fn drop(&mut self) {
        let _ = unsafe { self.mgr.UnregisterSessionNotification(&self.sink) };
    }
}

#[implement(IAudioVolumeDuckNotification)]
pub struct DuckEvents(pub Sender<Event>);

impl IAudioVolumeDuckNotification_Impl for DuckEvents_Impl {
    fn OnVolumeDuckNotification(&self, _session: &PCWSTR, _count: u32) -> windows::core::Result<()> {
        let _ = self.0.send(Event::Ducking(true));
        Ok(())
    }
    fn OnVolumeUnduckNotification(&self, _session: &PCWSTR) -> windows::core::Result<()> {
        let _ = self.0.send(Event::Ducking(false));
        Ok(())
    }
}

pub fn duck_events(tx: Sender<Event>) -> IAudioVolumeDuckNotification {
    DuckEvents(tx).into()
}

/// Watches one hushed session.
#[implement(IAudioSessionEvents)]
pub struct HushedEvents {
    instance: Arc<str>,
    tx: Sender<Event>,
    gain: Arc<Gain>,
    /// The volume Sidestage set (f32 bits).
    applied: Arc<AtomicU32>,
}

impl IAudioSessionEvents_Impl for HushedEvents_Impl {
    fn OnDisplayNameChanged(&self, _name: &PCWSTR, _ctx: *const GUID) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnIconPathChanged(&self, _path: &PCWSTR, _ctx: *const GUID) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnSimpleVolumeChanged(&self, volume: f32, mute: BOOL, ctx: *const GUID) -> windows::core::Result<()> {
        // SAFETY: Windows passes either null or a pointer to the context GUID of the change.
        if !ctx.is_null() && unsafe { *ctx } == SIDESTAGE_CONTEXT {
            return Ok(());
        }
        let applied = f32::from_bits(self.applied.load(Ordering::Relaxed));
        if (volume - applied).abs() > applied * 0.01 {
            // The capture now runs at a level the makeup gain does not match: silence it before
            // the next packet, the supervisor re-hushes and reopens.
            self.gain.close();
        }
        let _ = self.tx.send(Event::VolumeChanged { instance: self.instance.clone(), volume, muted: mute.as_bool() });
        Ok(())
    }
    fn OnChannelVolumeChanged(&self, _n: u32, _v: *const f32, _c: u32, _ctx: *const GUID) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnGroupingParamChanged(&self, _g: *const GUID, _ctx: *const GUID) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnStateChanged(&self, state: AudioSessionState) -> windows::core::Result<()> {
        if state == AudioSessionStateExpired {
            let _ = self.tx.send(Event::SessionGone { instance: self.instance.clone() });
        }
        Ok(())
    }
    fn OnSessionDisconnected(&self, _reason: AudioSessionDisconnectReason) -> windows::core::Result<()> {
        let _ = self.tx.send(Event::SessionGone { instance: self.instance.clone() });
        Ok(())
    }
}

pub fn hushed_events(
    instance: Arc<str>,
    tx: Sender<Event>,
    gain: Arc<Gain>,
    applied: Arc<AtomicU32>,
) -> IAudioSessionEvents {
    HushedEvents { instance, tx, gain, applied }.into()
}
