//! Audio processes: Core Audio's process objects plus the kernel facts (path, parent,
//! responsible process) that [`group`](super::group) uses to assemble apps.

use std::collections::HashMap;
use std::ffi::{c_int, c_void};
use std::mem::MaybeUninit;
use std::ptr;
use std::sync::OnceLock;

use anyhow::Context;
use objc2::rc::autoreleasepool;
use objc2_core_audio::{
    AudioObjectID, kAudioHardwarePropertyProcessObjectList, kAudioHardwarePropertyTranslatePIDToProcessObject,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeOutput, kAudioObjectUnknown,
    kAudioProcessPropertyBundleID, kAudioProcessPropertyDevices, kAudioProcessPropertyIsRunningOutput,
    kAudioProcessPropertyPID,
};
use objc2_foundation::{NSBundle, NSString};

use super::ca::{self, SYSTEM_OBJECT};
use super::devices;
use super::group::{AppGroup, Lookup, ProcFacts, SelfInfo, group_processes, outermost_app};
use super::sys::OsVersion;

const GLOBAL: u32 = kAudioObjectPropertyScopeGlobal;

/// Executable path of a process.
pub fn pid_path(pid: i32) -> Option<String> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer is as large as advertised.
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    let n = usize::try_from(n).ok().filter(|&n| n > 0)?;
    buf.truncate(n);
    String::from_utf8(buf).ok()
}

/// Parent PID (`proc_pidinfo(PROC_PIDTBSDINFO).pbi_ppid`; libc has no Apple `kinfo_proc`).
pub fn parent_pid(pid: i32) -> Option<i32> {
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = size_of::<libc::proc_bsdinfo>() as c_int;
    // SAFETY: the buffer is a proc_bsdinfo of the advertised size.
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, info.as_mut_ptr().cast(), size) };
    if n != size {
        return None;
    }
    // SAFETY: zero-initialized plain integers, filled in by the kernel.
    let info = unsafe { info.assume_init() };
    i32::try_from(info.pbi_ppid).ok()
}

type ResponsibleFn = unsafe extern "C" fn(libc::pid_t) -> libc::pid_t;

/// The process macOS holds responsible for `pid` (an XPC service's client app), via the
/// private `responsibility_get_pid_responsible_for_pid` (libquarantine, re-exported by
/// libSystem; used by Chromium and FineTune). Looked up at run time so a future macOS without
/// it only loses this grouping hint.
pub fn responsible_pid(pid: i32) -> Option<i32> {
    static FUNCTION: OnceLock<Option<ResponsibleFn>> = OnceLock::new();
    let f = FUNCTION.get_or_init(|| {
        // SAFETY: dlsym with a NUL-terminated symbol name.
        let sym = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"responsibility_get_pid_responsible_for_pid".as_ptr()) };
        // SAFETY: the symbol is `pid_t responsibility_get_pid_responsible_for_pid(pid_t)`.
        (!sym.is_null()).then(|| unsafe { std::mem::transmute::<*mut c_void, ResponsibleFn>(sym) })
    });
    // SAFETY: plain integer in and out.
    let r = unsafe { (*f)?(pid) };
    (r > 0).then_some(r)
}

/// `CFBundleIdentifier` of the bundle at `path`.
pub fn bundle_id_at(path: &str) -> Option<String> {
    autoreleasepool(|_| {
        let bundle = NSBundle::bundleWithPath(&NSString::from_str(path))?;
        bundle.bundleIdentifier().map(|id| id.to_string()).filter(|id| !id.is_empty())
    })
}

/// [`Lookup`] backed by the kernel and bundle Info.plists, caching bundle IDs.
#[derive(Default)]
pub struct SysLookup {
    bundle_ids: HashMap<String, String>,
}

impl Lookup for SysLookup {
    fn path(&mut self, pid: i32) -> Option<String> {
        pid_path(pid)
    }

    fn bundle_id(&mut self, app_path: &str) -> Option<String> {
        if let Some(id) = self.bundle_ids.get(app_path) {
            return Some(id.clone());
        }
        // Misses are not cached: an app being updated may be unreadable for a moment.
        let id = bundle_id_at(app_path)?;
        self.bundle_ids.insert(app_path.to_string(), id.clone());
        Some(id)
    }
}

/// Who Sidestage is, so it never lists or taps itself.
pub fn self_info(lookup: &mut SysLookup) -> SelfInfo {
    let pid = std::process::id() as i32;
    let exe = pid_path(pid).or_else(|| std::env::current_exe().ok().map(|p| p.to_string_lossy().into_owned()));
    let app_path = exe.as_deref().and_then(outermost_app).map(str::to_string);
    let key = app_path.as_deref().and_then(|app| lookup.bundle_id(app)).or(exe);
    SelfInfo { pid, app_path, key }
}

pub fn running_output(object: AudioObjectID) -> bool {
    ca::get_u32(object, kAudioProcessPropertyIsRunningOutput, GLOBAL).is_ok_and(|v| v != 0)
}

/// The Core Audio process object of a PID, if that process is a HAL client.
pub fn process_object_for_pid(pid: i32) -> Option<AudioObjectID> {
    // SAFETY: the qualifier is a pid_t, the value an AudioObjectID.
    let object: AudioObjectID = unsafe {
        ca::get_qualified(SYSTEM_OBJECT, kAudioHardwarePropertyTranslatePIDToProcessObject, GLOBAL, Some(&pid))
    }
    .ok()?;
    (object != kAudioObjectUnknown).then_some(object)
}

/// Every audio process with the facts needed to group it.
pub fn snapshot() -> anyhow::Result<Vec<ProcFacts>> {
    let objects = ca::get_ids(SYSTEM_OBJECT, kAudioHardwarePropertyProcessObjectList, GLOBAL)
        .context("listing audio processes")?;
    Ok(objects
        .into_iter()
        .filter_map(|object| {
            // A process that exited since the list was read fails here and is skipped.
            // SAFETY: a pid_t.
            let pid: i32 = unsafe { ca::get(object, kAudioProcessPropertyPID, GLOBAL) }.ok()?;
            Some(ProcFacts {
                object,
                pid,
                ppid: parent_pid(pid).unwrap_or(0),
                responsible: responsible_pid(pid).unwrap_or(0),
                bundle_id: ca::get_string(object, kAudioProcessPropertyBundleID, GLOBAL)
                    .ok()
                    .flatten()
                    .filter(|s| !s.is_empty()),
                path: pid_path(pid),
                running_output: running_output(object),
            })
        })
        .collect())
}

/// Apps with audio processes, Sidestage excluded.
pub fn app_groups(me: &SelfInfo, lookup: &mut SysLookup) -> anyhow::Result<Vec<AppGroup>> {
    Ok(group_processes(&snapshot()?, me, lookup))
}

/// Whether the app's processes currently play to `output`, and whether any plays to some other
/// output device.
pub fn device_usage(objects: &[AudioObjectID], output: AudioObjectID) -> (bool, bool) {
    let mut to_output = false;
    let mut elsewhere = false;
    for &object in objects {
        let ids = ca::get_ids(object, kAudioProcessPropertyDevices, kAudioObjectPropertyScopeOutput)
            .or_else(|_| ca::get_ids(object, kAudioProcessPropertyDevices, GLOBAL))
            .unwrap_or_default();
        for id in ids {
            if id == output {
                to_output = true;
            } else if devices::has_output(id) {
                elsewhere = true;
            }
        }
    }
    (to_output, elsewhere)
}

/// The macOS product version (`kern.osproductversion`).
pub fn os_version() -> Option<OsVersion> {
    let mut buf = [0u8; 64];
    let mut len = buf.len();
    // SAFETY: NUL-terminated name; the buffer and its length are valid.
    let rc = unsafe {
        libc::sysctlbyname(c"kern.osproductversion".as_ptr(), buf.as_mut_ptr().cast(), &mut len, ptr::null_mut(), 0)
    };
    if rc != 0 {
        return None;
    }
    OsVersion::parse(std::str::from_utf8(buf.get(..len)?).ok()?)
}
