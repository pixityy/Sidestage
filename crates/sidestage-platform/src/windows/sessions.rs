//! Render endpoints and their audio sessions (the rows of the Windows Volume mixer), and the
//! app list built from them. Everything here runs on a backend MTA thread.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use anyhow::Context;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::S_OK;
use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::{
    AudioSessionState, AudioSessionStateActive, AudioSessionStateExpired, DEVICE_STATE_ACTIVE, IAudioSessionControl,
    IAudioSessionControl2, IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator, ISimpleAudioVolume,
    MMDeviceEnumerator, eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, STGM_READ};
use windows::core::Interface;

use super::com::take_pwstr;
use super::procs::{Names, ProcessTable, app_key, exe_file_name};
use crate::{AppKey, AudioApp, OutputDevice};

pub fn enumerator() -> anyhow::Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.context("creating the audio device enumerator")
}

/// An active render endpoint.
pub struct Endpoint {
    pub id: String,
    pub name: String,
    pub device: IMMDevice,
}

pub fn device_id(dev: &IMMDevice) -> anyhow::Result<String> {
    Ok(take_pwstr(unsafe { dev.GetId() }.context("IMMDevice::GetId")?))
}

pub fn device_name(dev: &IMMDevice) -> String {
    let name = (|| unsafe {
        let store = dev.OpenPropertyStore(STGM_READ)?;
        let v = store.GetValue(&PKEY_Device_FriendlyName)?;
        windows::core::Result::Ok(v.to_string())
    })();
    match name {
        Ok(n) if !n.is_empty() => n,
        _ => "Unknown output".to_owned(),
    }
}

pub fn render_endpoints(en: &IMMDeviceEnumerator) -> anyhow::Result<Vec<Endpoint>> {
    let coll = unsafe { en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }.context("listing audio outputs")?;
    let n = unsafe { coll.GetCount() }?;
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        let Ok(device) = (unsafe { coll.Item(i) }) else {
            continue;
        };
        let Ok(id) = device_id(&device) else { continue };
        let name = device_name(&device);
        out.push(Endpoint { id, name, device });
    }
    Ok(out)
}

/// The system default output (console role, which is what "Default device" means in Sound
/// settings), or `None` if there is no output at all.
pub fn default_render(en: &IMMDeviceEnumerator) -> Option<IMMDevice> {
    unsafe { en.GetDefaultAudioEndpoint(eRender, eConsole) }.ok()
}

pub fn list_outputs(en: &IMMDeviceEnumerator) -> anyhow::Result<Vec<OutputDevice>> {
    let default_id = default_render(en).and_then(|d| device_id(&d).ok());
    Ok(render_endpoints(en)?
        .into_iter()
        .map(|e| OutputDevice { is_default: default_id.as_deref() == Some(e.id.as_str()), id: e.id, name: e.name })
        .collect())
}

/// The session manager of one endpoint.
pub struct Manager {
    pub endpoint_id: String,
    pub endpoint_name: String,
    pub mgr: IAudioSessionManager2,
}

/// Session managers for every active render endpoint, kept across scans because activating
/// them is the slow part of a scan.
#[derive(Default)]
pub struct Managers {
    pub list: Vec<Manager>,
}

impl Managers {
    /// Re-reads the endpoint list. Returns the managers of endpoints that disappeared and the
    /// indices (into `list`) of the ones that are new.
    pub fn refresh(&mut self, en: &IMMDeviceEnumerator) -> anyhow::Result<(Vec<Manager>, Vec<usize>)> {
        let endpoints = render_endpoints(en)?;
        let live: HashSet<&str> = endpoints.iter().map(|e| e.id.as_str()).collect();
        let (keep, gone): (Vec<Manager>, Vec<Manager>) =
            std::mem::take(&mut self.list).into_iter().partition(|m| live.contains(m.endpoint_id.as_str()));
        self.list = keep;
        let mut added = Vec::new();
        for e in endpoints {
            if self.list.iter().any(|m| m.endpoint_id == e.id) {
                continue;
            }
            match unsafe { e.device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) } {
                Ok(mgr) => {
                    added.push(self.list.len());
                    self.list.push(Manager { endpoint_id: e.id, endpoint_name: e.name, mgr });
                }
                Err(err) => log::debug!("no session manager for output {}: {err}", e.name),
            }
        }
        Ok((gone, added))
    }
}

/// One audio session (one app's stream group on one endpoint).
#[derive(Clone)]
pub struct SessionRef {
    pub ctl: IAudioSessionControl,
    pub pid: u32,
    pub endpoint_id: String,
    pub endpoint_name: String,
    /// Stable per app and endpoint; Windows persists volume under it.
    pub session_id: String,
    /// Unique per session instance.
    pub instance_id: String,
    pub state: AudioSessionState,
}

impl SessionRef {
    pub fn volume(&self) -> windows::core::Result<ISimpleAudioVolume> {
        self.ctl.cast()
    }

    /// Pre-volume peak of the session's audio.
    pub fn peak(&self) -> Option<f32> {
        let m: IAudioMeterInformation = self.meter().ok()?;
        unsafe { m.GetPeakValue() }.ok()
    }

    pub fn meter(&self) -> windows::core::Result<IAudioMeterInformation> {
        self.ctl.cast()
    }

    pub fn display_name(&self) -> String {
        unsafe { self.ctl.GetDisplayName() }.map(take_pwstr).unwrap_or_default()
    }

    pub fn active(&self) -> bool {
        self.state == AudioSessionStateActive
    }
}

/// All non-system, non-expired sessions on all endpoints in `managers`.
pub fn scan(managers: &Managers) -> Vec<SessionRef> {
    let mut out = Vec::new();
    for m in &managers.list {
        let Ok(en) = (unsafe { m.mgr.GetSessionEnumerator() }) else {
            continue;
        };
        let n = unsafe { en.GetCount() }.unwrap_or(0);
        for i in 0..n {
            let Ok(ctl) = (unsafe { en.GetSession(i) }) else {
                continue;
            };
            let Ok(ctl2) = ctl.cast::<IAudioSessionControl2>() else {
                continue;
            };
            // S_FALSE (1) means "not the system sounds session", so compare with S_OK exactly.
            if unsafe { ctl2.IsSystemSoundsSession() } == S_OK {
                continue;
            }
            let state = unsafe { ctl.GetState() }.unwrap_or(AudioSessionStateExpired);
            if state == AudioSessionStateExpired {
                continue;
            }
            // AUDCLNT_S_NO_SINGLE_PROCESS is a success code; the creator PID is still useful.
            let pid = unsafe { ctl2.GetProcessId() }.unwrap_or(0);
            if pid == 0 {
                continue;
            }
            let session_id = unsafe { ctl2.GetSessionIdentifier() }.map(take_pwstr).unwrap_or_default();
            let instance_id = unsafe { ctl2.GetSessionInstanceIdentifier() }.map(take_pwstr).unwrap_or_default();
            out.push(SessionRef {
                ctl,
                pid,
                endpoint_id: m.endpoint_id.clone(),
                endpoint_name: m.endpoint_name.clone(),
                session_id,
                instance_id,
                state,
            });
        }
    }
    out
}

/// Each session's meter peak, or `None` where the meter cannot be trusted to show what the app
/// plays.
///
/// Session meters are taken before the session volume. A process that runs a process-loopback
/// capture (Sidestage, a screen recorder, a voice chat sharing its screen) also gets a session
/// on the captured audio's endpoint, and its meter shows the captured audio (measured on build
/// 26200: a process playing silence while capturing a 0.01 tone read 0.0100), which would make
/// a silent app look like it is playing. Identical non-zero peaks from different processes on
/// one endpoint are treated as such a mirrored meter. Sidestage's own sessions are known
/// mirrors (of its sources and of the apps ducking listens to), so matching one of them does not
/// make the app's real meter untrusted.
pub fn trusted_peaks(sessions: &[SessionRef]) -> Vec<Option<f32>> {
    let raw: Vec<Option<f32>> = sessions.iter().map(SessionRef::peak).collect();
    let keys: Vec<(u32, &str)> = sessions.iter().map(|s| (s.pid, s.endpoint_id.as_str())).collect();
    unmirrored(&keys, &raw, std::process::id())
}

/// See [`trusted_peaks`]: `keys` are each session's (process, endpoint), `me` is Sidestage.
fn unmirrored(keys: &[(u32, &str)], raw: &[Option<f32>], me: u32) -> Vec<Option<f32>> {
    keys.iter()
        .zip(raw)
        .map(|(&(pid, ep), &p)| {
            let mirrored = p.is_some_and(|p| p > 0.0)
                && keys.iter().zip(raw).any(|(&(o, oep), &q)| o != pid && o != me && oep == ep && q == p);
            if mirrored { None } else { p }
        })
        .collect()
}

/// Builds the picker's app list from the current sessions: sessions are grouped by their app
/// root, keyed by the root's executable path. Sidestage itself and every ancestor of it are left
/// out, because capturing a tree that contains Sidestage would feed its own output back.
pub fn collect_apps(managers: &Managers, names: &mut Names) -> anyhow::Result<Vec<AudioApp>> {
    let sessions = scan(managers);
    let peaks = trusted_peaks(&sessions);
    let mut procs = ProcessTable::snapshot()?;
    let blocked: HashSet<u32> = procs.lineage(std::process::id()).into_iter().collect();

    struct Group {
        path: String,
        pids: BTreeSet<u32>,
        root_display: String,
        any_display: String,
        active: bool,
        /// Loudest trusted meter reading of an active session.
        peak: f32,
        /// An active session's meter could not be trusted.
        unknown: bool,
    }
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    for (s, peak) in sessions.iter().zip(peaks) {
        if blocked.contains(&s.pid) {
            continue;
        }
        let root = procs.root_of(s.pid);
        if blocked.contains(&root) {
            continue;
        }
        let Some(path) = procs.path(root) else {
            continue;
        };
        let g = groups.entry(app_key(&path)).or_insert_with(|| Group {
            path: path.clone(),
            pids: BTreeSet::new(),
            root_display: String::new(),
            any_display: String::new(),
            active: false,
            peak: 0.0,
            unknown: false,
        });
        g.pids.insert(root);
        g.pids.insert(s.pid);
        let display = s.display_name();
        if !display.is_empty() {
            if s.pid == root && g.root_display.is_empty() {
                g.root_display = display.clone();
            }
            if g.any_display.is_empty() {
                g.any_display = display;
            }
        }
        if s.active() {
            g.active = true;
            match peak {
                Some(p) => g.peak = g.peak.max(p),
                None => g.unknown = true,
            }
        }
    }

    let mut apps: Vec<AudioApp> = groups
        .into_iter()
        .map(|(key, g)| {
            let display = if g.root_display.is_empty() { g.any_display } else { g.root_display };
            let audible = g.unknown || g.peak > 1e-4;
            AudioApp {
                key: AppKey(key),
                name: names.app_name(&display, &g.path),
                detail: exe_file_name(&g.path),
                pids: g.pids.into_iter().collect(),
                playing: g.active && audible,
                peak: (!g.unknown).then_some(g.peak.min(1.0)),
            }
        })
        .collect();
    apps.sort_by(|a, b| b.playing.cmp(&a.playing).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(apps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirrored_meters_are_not_trusted() {
        const ME: u32 = 1;
        // A capturing app (pid 3) mirrors the game (pid 2); Sidestage (pid 1) mirrors it too.
        let keys = [(2, "ep"), (3, "ep"), (ME, "ep"), (4, "ep"), (5, "other")];
        let raw = [Some(0.4), Some(0.4), Some(0.4), Some(0.0), Some(0.4)];
        let t = unmirrored(&keys, &raw, ME);
        assert_eq!(t[0], None, "the game matches another app's mirror");
        assert_eq!(t[1], None, "so does the capturing app");
        assert_eq!(t[3], Some(0.0), "silence is never a mirror");
        assert_eq!(t[4], Some(0.4), "another endpoint is not compared");
        // Only Sidestage mirrors the game: Sidestage's own sessions do not count against it.
        let t = unmirrored(&[(2, "ep"), (ME, "ep")], &[Some(0.4), Some(0.4)], ME);
        assert_eq!(t[0], Some(0.4));
    }
}
