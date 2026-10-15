//! Processes: the Toolhelp process table, app roots and trees, executable paths and the
//! human-readable app names shown in the picker.
//!
//! Many apps play audio from a helper process (Chromium's audio service, Discord's voice
//! process). Process loopback in `INCLUDE_TARGET_PROCESS_TREE` mode captures a whole tree, so
//! apps are identified by their *root*: the highest ancestor with the same executable. Parent
//! PIDs in Toolhelp can be stale (the parent exited and its PID was reused), so every parent
//! link is checked against process creation times.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Shell::SHLoadIndirectString;
use windows::core::{HSTRING, PWSTR, w};

use super::com::{Handle, created_of, open_process};

/// One row of the process table.
#[derive(Clone, Debug)]
pub struct Proc {
    pub ppid: u32,
    /// Executable file name, lowercase (e.g. `spotify.exe`).
    pub exe_name: String,
}

/// Facts about a process that need a handle (cached per table, filled lazily).
#[derive(Clone, Debug, Default)]
struct Info {
    created: Option<u64>,
    path: Option<String>,
}

/// A snapshot of all processes with lazily resolved creation times and paths.
pub struct ProcessTable {
    procs: HashMap<u32, Proc>,
    info: HashMap<u32, Info>,
}

impl ProcessTable {
    pub fn snapshot() -> anyhow::Result<Self> {
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }?;
        // SAFETY: the snapshot handle is owned here and closed on drop.
        let snap = unsafe { Handle::from_raw(snap) };
        let mut procs = HashMap::with_capacity(512);
        let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if unsafe { Process32FirstW(snap.raw(), &mut e) }.is_ok() {
            loop {
                let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                let exe_name = String::from_utf16_lossy(&e.szExeFile[..n]).to_lowercase();
                procs.insert(e.th32ProcessID, Proc { ppid: e.th32ParentProcessID, exe_name });
                if unsafe { Process32NextW(snap.raw(), &mut e) }.is_err() {
                    break;
                }
            }
        }
        Ok(ProcessTable { procs, info: HashMap::new() })
    }

    pub fn get(&self, pid: u32) -> Option<&Proc> {
        self.procs.get(&pid)
    }

    pub fn pids(&self) -> impl Iterator<Item = u32> + '_ {
        self.procs.keys().copied()
    }

    fn info(&mut self, pid: u32) -> &Info {
        self.info.entry(pid).or_insert_with(|| match open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION) {
            Some(h) => Info { created: created_of(&h), path: image_path(&h) },
            None => Info::default(),
        })
    }

    pub fn created(&mut self, pid: u32) -> Option<u64> {
        self.info(pid).created
    }

    /// Full Win32 path of the executable, in its original case.
    pub fn path(&mut self, pid: u32) -> Option<String> {
        self.info(pid).path.clone()
    }

    /// The parent of `pid`, if it really is the parent (it was created before the child).
    pub fn parent(&mut self, pid: u32) -> Option<u32> {
        let ppid = self.procs.get(&pid)?.ppid;
        if ppid == 0 || ppid == pid || !self.procs.contains_key(&ppid) {
            return None;
        }
        let child = self.created(pid)?;
        let parent = self.created(ppid)?;
        (parent <= child).then_some(ppid)
    }

    /// The app root of `pid`: walk up while the parent runs the same executable.
    pub fn root_of(&mut self, pid: u32) -> u32 {
        let mut cur = pid;
        for _ in 0..64 {
            let Some(name) = self.procs.get(&cur).map(|p| p.exe_name.clone()) else {
                break;
            };
            match self.parent(cur) {
                Some(pp) if self.procs.get(&pp).is_some_and(|p| p.exe_name == name) => cur = pp,
                _ => break,
            }
        }
        cur
    }

    /// `pid` and every verified ancestor of it.
    pub fn lineage(&mut self, pid: u32) -> Vec<u32> {
        let mut out = vec![pid];
        let mut cur = pid;
        for _ in 0..64 {
            match self.parent(cur) {
                Some(pp) if !out.contains(&pp) => {
                    out.push(pp);
                    cur = pp;
                }
                _ => break,
            }
        }
        out
    }

    /// True if `pid` is `root` or a verified descendant of it.
    pub fn is_in_tree(&mut self, pid: u32, root: u32) -> bool {
        self.lineage(pid).contains(&root)
    }

    /// `root` and all its verified descendants.
    pub fn tree(&mut self, root: u32) -> HashSet<u32> {
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for (&pid, p) in &self.procs {
            children.entry(p.ppid).or_default().push(pid);
        }
        let mut out = HashSet::from([root]);
        let mut stack = vec![root];
        while let Some(cur) = stack.pop() {
            for &c in children.get(&cur).map(Vec::as_slice).unwrap_or_default() {
                if !out.contains(&c) && self.parent(c) == Some(cur) {
                    out.insert(c);
                    stack.push(c);
                }
            }
        }
        out
    }
}

/// Full Win32 path of a process's executable.
pub fn image_path(h: &Handle) -> Option<String> {
    let mut buf = vec![0u16; 1024];
    loop {
        let mut len = buf.len() as u32;
        let r = unsafe { QueryFullProcessImageNameW(h.raw(), PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) };
        match r {
            Ok(()) => return Some(String::from_utf16_lossy(&buf[..len as usize])),
            // Paths can exceed MAX_PATH; grow once or twice, then give up.
            Err(_) if buf.len() < 32 * 1024 => buf.resize(buf.len() * 4, 0),
            Err(_) => return None,
        }
    }
}

/// Executable path of a PID (no table needed).
pub fn pid_path(pid: u32) -> Option<String> {
    image_path(&open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)?)
}

/// The key the backend uses for an app: its full executable path, lowercase.
pub fn app_key(path: &str) -> String {
    path.to_lowercase()
}

/// An app key with its version folder taken out, for recognizing the same app after an update.
/// Two common installers move the executable to a new versioned folder on every update:
/// * Store apps: `...\WindowsApps\<name>_<version>_<arch>_<res>_<publisher>\app.exe`;
/// * Squirrel apps (Discord, Slack, TIDAL ...): `...\<app>\app-<version>\app.exe`.
pub fn match_key(key: &str) -> String {
    let key = key.to_lowercase();
    let mut parent_is_store = false;
    let segments: Vec<String> = key
        .split('\\')
        .map(|seg| {
            let store_package = std::mem::replace(&mut parent_is_store, seg == "windowsapps");
            if store_package {
                let mut fields: Vec<&str> = seg.split('_').collect();
                if fields.len() >= 5 {
                    fields[1] = "*";
                    return fields.join("_");
                }
            }
            match seg.strip_prefix("app-") {
                Some(v) if !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.') => "app-*".to_owned(),
                _ => seg.to_owned(),
            }
        })
        .collect();
    segments.join("\\")
}

/// Caches the slow parts of naming apps (version resources, indirect strings).
#[derive(Default)]
pub struct Names {
    descriptions: HashMap<String, Option<String>>,
    indirect: HashMap<String, Option<String>>,
}

impl Names {
    /// Display name for an app: the session's display name (with `@dll,-id` / `@{...}`
    /// references resolved), else the executable's FileDescription, else its file stem.
    pub fn app_name(&mut self, display_name: &str, exe_path: &str) -> String {
        let display = display_name.trim();
        if !display.is_empty() {
            if !display.starts_with('@') {
                return display.to_owned();
            }
            let resolved = self.indirect.entry(display.to_owned()).or_insert_with(|| resolve_indirect(display)).clone();
            if let Some(s) = resolved.filter(|s| !s.trim().is_empty()) {
                return s;
            }
        }
        let desc =
            self.descriptions.entry(exe_path.to_lowercase()).or_insert_with(|| file_description(exe_path)).clone();
        desc.unwrap_or_else(|| exe_stem(exe_path))
    }
}

pub fn exe_stem(path: &str) -> String {
    Path::new(path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_owned())
}

pub fn exe_file_name(path: &str) -> String {
    Path::new(path).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_owned())
}

fn resolve_indirect(source: &str) -> Option<String> {
    let mut buf = vec![0u16; 1024];
    unsafe { SHLoadIndirectString(&HSTRING::from(source), &mut buf, None) }.ok()?;
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..n]))
}

/// The FileDescription from an executable's version resource ("Spotify", "Google Chrome").
fn file_description(path: &str) -> Option<String> {
    let wpath = HSTRING::from(path);
    let size = unsafe { GetFileVersionInfoSizeW(&wpath, None) };
    if size == 0 {
        return None;
    }
    let mut block = vec![0u8; size as usize];
    unsafe { GetFileVersionInfoW(&wpath, None, size, block.as_mut_ptr().cast()) }.ok()?;

    let mut translations: Vec<(u16, u16)> = Vec::new();
    let mut p: *mut core::ffi::c_void = std::ptr::null_mut();
    let mut len = 0u32;
    if unsafe { VerQueryValueW(block.as_ptr().cast(), w!("\\VarFileInfo\\Translation"), &mut p, &mut len) }.as_bool()
        && !p.is_null()
    {
        for i in 0..(len as usize / 4) {
            // SAFETY: VerQueryValueW returned `len` bytes at `p` inside `block`.
            let (lang, cp) = unsafe {
                let q = (p as *const u16).add(i * 2);
                (q.read_unaligned(), q.add(1).read_unaligned())
            };
            translations.push((lang, cp));
        }
    }
    translations.extend([(0x0409, 0x04b0), (0x0409, 0x04e4), (0x0000, 0x04b0)]);
    for (lang, cp) in translations {
        let key = HSTRING::from(format!("\\StringFileInfo\\{lang:04x}{cp:04x}\\FileDescription"));
        let mut p: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut len = 0u32;
        if unsafe { VerQueryValueW(block.as_ptr().cast(), &key, &mut p, &mut len) }.as_bool() && !p.is_null() && len > 0
        {
            // SAFETY: `len` UTF-16 units at `p`, inside `block`.
            let units: Vec<u16> =
                (0..len as usize).map(|i| unsafe { (p as *const u16).add(i).read_unaligned() }).collect();
            let n = units.iter().position(|&c| c == 0).unwrap_or(units.len());
            let s = String::from_utf16_lossy(&units[..n]).trim().to_owned();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_tree() {
        let me = std::process::id();
        let mut t = ProcessTable::snapshot().unwrap();
        let lineage = t.lineage(me);
        assert_eq!(lineage[0], me);
        assert!(t.is_in_tree(me, me));
        // Our root runs the same executable as we do.
        let root = t.root_of(me);
        assert!(lineage.contains(&root));
        assert_eq!(t.get(root).unwrap().exe_name, t.get(me).unwrap().exe_name);
        assert!(t.tree(root).contains(&me));
        let path = t.path(me).unwrap();
        assert_eq!(app_key(&path), std::env::current_exe().unwrap().to_string_lossy().to_lowercase());
    }

    #[test]
    fn names_fall_back_to_the_file_stem() {
        let mut n = Names::default();
        assert_eq!(n.app_name("Spotify", r"c:\x\spotify.exe"), "Spotify");
        assert_eq!(n.app_name("", r"c:\no\such\Thing.exe"), "Thing");
        assert_eq!(exe_file_name(r"c:\x\Spotify.exe"), "Spotify.exe");
    }

    #[test]
    fn store_app_updates_keep_their_match_key() {
        let old = r"C:\Program Files\WindowsApps\SpotifyAB.SpotifyMusic_1.298.301.0_x64__zpdnekdrzrea0\Spotify.exe";
        let new = r"c:\program files\windowsapps\spotifyab.spotifymusic_1.304.258.0_x64__zpdnekdrzrea0\spotify.exe";
        assert_eq!(match_key(old), match_key(new));
        assert_eq!(
            match_key(new),
            r"c:\program files\windowsapps\spotifyab.spotifymusic_*_x64__zpdnekdrzrea0\spotify.exe"
        );
        let other = r"c:\program files\windowsapps\otherco.player_1.0.0.0_x64__abc\spotify.exe";
        assert_ne!(match_key(other), match_key(new));
        // Ordinary paths are only lowercased.
        assert_eq!(match_key(r"C:\Apps\Foo_1_2_3_4\foo.exe"), r"c:\apps\foo_1_2_3_4\foo.exe");
        assert_eq!(match_key(&match_key(new)), match_key(new), "idempotent");
    }

    #[test]
    fn squirrel_app_updates_keep_their_match_key() {
        let old = r"C:\Users\me\AppData\Local\Discord\app-1.0.9261\Discord.exe";
        let new = r"c:\users\me\appdata\local\discord\app-1.0.9302\discord.exe";
        assert_eq!(match_key(old), match_key(new));
        assert_eq!(match_key(new), r"c:\users\me\appdata\local\discord\app-*\discord.exe");
        // Not a version folder, and different apps stay different.
        assert_eq!(match_key(r"c:\tools\app-store\x.exe"), r"c:\tools\app-store\x.exe");
        assert_ne!(match_key(r"c:\a\app-1\x.exe"), match_key(r"c:\b\app-1\x.exe"));
    }
}
