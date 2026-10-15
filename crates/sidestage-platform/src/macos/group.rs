//! Which audio processes belong to which app (pure logic, unit-tested on every OS).
//!
//! Core Audio lists one process object per process that talks to the HAL, and many apps play
//! from helpers: Chromium browsers and Electron apps from helper apps nested in their bundle,
//! Safari from WebKit XPC services that launchd starts on Safari's behalf. The tap has to cover
//! all of them, or part of the music is neither captured nor muted.
//!
//! A process belongs to:
//! 1. the outermost `.app` bundle its executable lives in (nested helper apps);
//! 2. else, if launchd started it (parent PID 1), the app *responsible* for it
//!    (`responsibility_get_pid_responsible_for_pid`; XPC services such as WebKit's);
//! 3. else, its parent's app when its bundle ID is a sub-bundle of that app's
//!    (`com.example.app.helper` started by `com.example.app`);
//! 4. else itself (a command-line player such as `afplay` or `mpv`).
//!
//! Finally a standalone process whose bundle ID is a sub-bundle of a listed app joins that app.
//! Responsibility and parents are deliberately not followed any further: a player started from
//! Terminal is "responsible" to Terminal and its parent chain ends in Terminal, and grouping it
//! there would capture the wrong thing.

use std::collections::BTreeMap;

/// What Core Audio and the kernel say about one process object.
#[derive(Clone, Debug, Default)]
pub struct ProcFacts {
    /// The Core Audio process object (`AudioObjectID`).
    pub object: u32,
    pub pid: i32,
    /// Parent PID, 0 if unknown.
    pub ppid: i32,
    /// PID of the process macOS holds responsible for this one, 0 if unknown.
    pub responsible: i32,
    pub bundle_id: Option<String>,
    /// Executable path.
    pub path: Option<String>,
    pub running_output: bool,
}

/// Facts about processes and bundles that are not necessarily audio clients themselves.
pub trait Lookup {
    /// Executable path of any process.
    fn path(&mut self, pid: i32) -> Option<String>;
    /// `CFBundleIdentifier` of the bundle at `app_path`.
    fn bundle_id(&mut self, app_path: &str) -> Option<String>;
}

/// Sidestage itself, which must never be offered or tapped.
#[derive(Clone, Debug, Default)]
pub struct SelfInfo {
    pub pid: i32,
    /// Outermost `.app` bundle Sidestage runs from, if bundled.
    pub app_path: Option<String>,
    /// Sidestage's own app key (bundle ID or executable path).
    pub key: Option<String>,
}

/// One app with all of its audio processes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppGroup {
    /// Bundle ID, or the executable path for unbundled programs.
    pub key: String,
    pub name: String,
    pub detail: String,
    /// The app bundle, when the app is one.
    pub app_path: Option<String>,
    pub pids: Vec<u32>,
    /// Core Audio process objects, the tap's process list.
    pub objects: Vec<u32>,
    pub playing: bool,
}

impl AppGroup {
    /// System agents (Siri, Control Center, `coreaudiod` clients in `/usr/libexec`, ...) are
    /// audio clients too; they are only worth listing while they actually play.
    pub fn is_user_facing(&self) -> bool {
        match &self.app_path {
            Some(path) => !path.starts_with("/System/Library/") && !path.starts_with("/Library/Apple/"),
            None => false,
        }
    }
}

/// The outermost `X.app` directory in `path`, e.g. `/Applications/Google Chrome.app` for a
/// helper deep inside Chrome's bundle.
pub fn outermost_app(path: &str) -> Option<&str> {
    let mut end = 0;
    for component in path.split('/') {
        end += component.len();
        let is_app = component.len() > 4
            && component.get(component.len() - 4..).is_some_and(|ext| ext.eq_ignore_ascii_case(".app"));
        if is_app {
            return Some(&path[..end]);
        }
        end += 1; // the '/'
    }
    None
}

/// `id` is `main` or one of its sub-bundles (`main.something`); `main.other-app` style
/// prefixes such as `com.example.app-beta` do not count.
pub fn is_same_or_sub_bundle(id: &str, main: &str) -> bool {
    id == main || (id.len() > main.len() && id.starts_with(main) && id.as_bytes()[main.len()] == b'.')
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(path)
}

fn app_display_name(app_path: &str) -> String {
    let name = file_name(app_path);
    name.get(..name.len().saturating_sub(4)).filter(|s| !s.is_empty()).unwrap_or(name).to_string()
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Owner {
    /// An app bundle, by path.
    App(String),
    /// A standalone process, by its provisional key.
    Solo(String),
}

#[derive(Default)]
struct Builder {
    objects: Vec<u32>,
    pids: Vec<u32>,
    playing: bool,
    /// CoreAudio bundle ID of the bundle's main executable, if it is an audio client.
    main_bundle_id: Option<String>,
    solo_path: Option<String>,
    solo_bundle_id: Option<String>,
}

fn owner_of(p: &ProcFacts, me: &SelfInfo, lookup: &mut dyn Lookup) -> Option<Owner> {
    if p.pid == me.pid {
        return None;
    }
    if let Some(app) = p.path.as_deref().and_then(outermost_app) {
        return Some(Owner::App(app.to_string()));
    }
    let r = p.responsible;
    if p.ppid == 1 && r > 1 && r != p.pid {
        if r == me.pid {
            return None; // an XPC service working for Sidestage
        }
        if let Some(app) = lookup.path(r).as_deref().and_then(outermost_app) {
            return Some(Owner::App(app.to_string()));
        }
    }
    if p.ppid > 1 {
        if p.ppid == me.pid {
            return None;
        }
        if let Some(id) = p.bundle_id.as_deref()
            && let Some(app) = lookup.path(p.ppid).as_deref().and_then(outermost_app)
            && lookup.bundle_id(app).is_some_and(|main| is_same_or_sub_bundle(id, &main))
        {
            return Some(Owner::App(app.to_string()));
        }
    }
    let key = p.bundle_id.clone().or_else(|| p.path.clone()).unwrap_or_else(|| format!("pid {}", p.pid));
    Some(Owner::Solo(key))
}

/// Groups audio processes into apps, excluding Sidestage itself. Sorted by name.
pub fn group_processes(procs: &[ProcFacts], me: &SelfInfo, lookup: &mut dyn Lookup) -> Vec<AppGroup> {
    let mut builders: BTreeMap<Owner, Builder> = BTreeMap::new();
    for p in procs {
        let Some(owner) = owner_of(p, me, lookup) else {
            continue;
        };
        if let (Owner::App(app), Some(own)) = (&owner, &me.app_path)
            && app == own
        {
            continue;
        }
        let b = builders.entry(owner.clone()).or_default();
        b.objects.push(p.object);
        if let Ok(pid) = u32::try_from(p.pid) {
            b.pids.push(pid);
        }
        b.playing |= p.running_output;
        match &owner {
            Owner::App(app) => {
                let is_main = p.path.as_deref().is_some_and(|path| {
                    path.strip_prefix(app.as_str()).is_some_and(|rest| rest.starts_with("/Contents/MacOS/"))
                });
                if is_main && b.main_bundle_id.is_none() {
                    b.main_bundle_id = p.bundle_id.clone();
                }
            }
            Owner::Solo(_) => {
                b.solo_path = b.solo_path.take().or_else(|| p.path.clone());
                b.solo_bundle_id = b.solo_bundle_id.take().or_else(|| p.bundle_id.clone());
            }
        }
    }

    // Name and key every group; merge groups that end up with the same key.
    let mut apps: BTreeMap<String, AppGroup> = BTreeMap::new();
    let mut solos: Vec<AppGroup> = Vec::new();
    for (owner, b) in builders {
        match owner {
            Owner::App(app_path) => {
                let key = lookup
                    .bundle_id(&app_path)
                    .or(b.main_bundle_id)
                    .filter(|k| !k.is_empty())
                    .unwrap_or_else(|| app_path.clone());
                let group = AppGroup {
                    key: key.clone(),
                    name: app_display_name(&app_path),
                    detail: String::new(),
                    app_path: Some(app_path),
                    pids: b.pids,
                    objects: b.objects,
                    playing: b.playing,
                };
                match apps.get_mut(&key) {
                    Some(existing) => merge(existing, group),
                    None => {
                        apps.insert(key, group);
                    }
                }
            }
            Owner::Solo(key) => {
                let name = match (&b.solo_path, &b.solo_bundle_id) {
                    (Some(path), _) => file_name(path).to_string(),
                    (None, Some(id)) => id.rsplit('.').next().unwrap_or(id).to_string(),
                    (None, None) => key.clone(),
                };
                solos.push(AppGroup {
                    key,
                    name,
                    detail: b.solo_path.unwrap_or_default(),
                    app_path: None,
                    pids: b.pids,
                    objects: b.objects,
                    playing: b.playing,
                });
            }
        }
    }

    let mut out: Vec<AppGroup> = Vec::new();
    for solo in solos {
        let parent = apps.values_mut().find(|app| is_same_or_sub_bundle(&solo.key, &app.key));
        match parent {
            Some(app) => merge(app, solo),
            None => out.push(solo),
        }
    }
    out.extend(apps.into_values());
    out.retain(|g| me.key.as_deref() != Some(g.key.as_str()));
    for g in &mut out {
        g.pids.sort_unstable();
        g.pids.dedup();
        g.objects.sort_unstable();
        g.objects.dedup();
        if g.app_path.is_some() {
            g.detail =
                if g.objects.len() > 1 { format!("{} · {} processes", g.key, g.objects.len()) } else { g.key.clone() };
        }
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.key.cmp(&b.key)));
    out
}

fn merge(into: &mut AppGroup, from: AppGroup) {
    into.pids.extend(from.pids);
    into.objects.extend(from.objects);
    into.playing |= from.playing;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Fake {
        paths: HashMap<i32, String>,
        bundles: HashMap<String, String>,
    }

    impl Lookup for Fake {
        fn path(&mut self, pid: i32) -> Option<String> {
            self.paths.get(&pid).cloned()
        }
        fn bundle_id(&mut self, app_path: &str) -> Option<String> {
            self.bundles.get(app_path).cloned()
        }
    }

    fn proc(object: u32, pid: i32, ppid: i32, path: &str, bundle: Option<&str>, playing: bool) -> ProcFacts {
        ProcFacts {
            object,
            pid,
            ppid,
            responsible: pid,
            bundle_id: bundle.map(str::to_string),
            path: Some(path.to_string()),
            running_output: playing,
        }
    }

    fn me() -> SelfInfo {
        SelfInfo {
            pid: 9999,
            app_path: Some("/Applications/Sidestage.app".into()),
            key: Some("com.pixityy.sidestage".into()),
        }
    }

    #[test]
    fn finds_bundles() {
        assert_eq!(
            outermost_app("/Applications/Spotify.app/Contents/MacOS/Spotify"),
            Some("/Applications/Spotify.app")
        );
        assert_eq!(
            outermost_app(
                "/Applications/Google Chrome.app/Contents/Frameworks/G.framework/Helpers/Google Chrome Helper.app/Contents/MacOS/Google Chrome Helper"
            ),
            Some("/Applications/Google Chrome.app")
        );
        assert_eq!(outermost_app("/Applications/Music.APP"), Some("/Applications/Music.APP"));
        assert_eq!(outermost_app("/opt/homebrew/bin/mpv"), None);
        assert_eq!(outermost_app("/usr/bin/.app"), None);
        assert!(is_same_or_sub_bundle("org.chromium.Chromium.helper", "org.chromium.Chromium"));
        assert!(is_same_or_sub_bundle("com.spotify.client", "com.spotify.client"));
        assert!(!is_same_or_sub_bundle("com.example.app-other", "com.example.app"));
        assert!(!is_same_or_sub_bundle("com.example", "com.example.app"));
        assert_eq!(app_display_name("/Applications/Google Chrome.app"), "Google Chrome");
    }

    #[test]
    fn chromium_helpers_join_their_browser() {
        let chrome = "/Applications/Google Chrome.app";
        let procs = [
            proc(10, 100, 1, &format!("{chrome}/Contents/MacOS/Google Chrome"), Some("com.google.Chrome"), false),
            proc(
                11,
                101,
                100,
                &format!(
                    "{chrome}/Contents/Frameworks/F.framework/Helpers/Google Chrome Helper.app/Contents/MacOS/Google Chrome Helper"
                ),
                Some("com.google.Chrome.helper"),
                true,
            ),
            proc(
                20,
                200,
                1,
                "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
                Some("com.google.Chrome.canary"),
                false,
            ),
        ];
        let mut lookup = Fake::default();
        lookup.bundles.insert(chrome.into(), "com.google.Chrome".into());
        lookup.bundles.insert("/Applications/Google Chrome Canary.app".into(), "com.google.Chrome.canary".into());
        let groups = group_processes(&procs, &me(), &mut lookup);
        assert_eq!(groups.len(), 2, "{groups:#?}");
        let g = &groups[0];
        assert_eq!(g.key, "com.google.Chrome");
        assert_eq!(g.name, "Google Chrome");
        assert_eq!(g.pids, vec![100, 101]);
        assert_eq!(g.objects, vec![10, 11]);
        assert!(g.playing);
        assert_eq!(g.detail, "com.google.Chrome · 2 processes");
        assert_eq!(groups[1].key, "com.google.Chrome.canary", "Canary is its own app, not a Chrome helper");
        assert!(!groups[1].playing);
    }

    #[test]
    fn xpc_services_join_the_responsible_app() {
        let mut gpu = proc(
            30,
            300,
            1,
            "/System/Library/Frameworks/WebKit.framework/Versions/A/XPCServices/com.apple.WebKit.GPU.xpc/Contents/MacOS/com.apple.WebKit.GPU",
            Some("com.apple.WebKit.GPU"),
            true,
        );
        gpu.responsible = 150;
        let mut lookup = Fake::default();
        lookup.paths.insert(150, "/Applications/Safari.app/Contents/MacOS/Safari".into());
        lookup.bundles.insert("/Applications/Safari.app".into(), "com.apple.Safari".into());
        let groups = group_processes(&[gpu], &me(), &mut lookup);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].key, "com.apple.Safari");
        assert_eq!(groups[0].name, "Safari");
        assert_eq!(groups[0].pids, vec![300]);
        assert!(groups[0].is_user_facing());
    }

    #[test]
    fn terminal_players_stay_standalone() {
        let mut mpv = proc(40, 400, 399, "/opt/homebrew/bin/mpv", None, true);
        mpv.responsible = 50; // Terminal
        let mut lookup = Fake::default();
        lookup.paths.insert(50, "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal".into());
        lookup.paths.insert(399, "/bin/zsh".into());
        let groups = group_processes(&[mpv], &me(), &mut lookup);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].key, "/opt/homebrew/bin/mpv");
        assert_eq!(groups[0].name, "mpv");
        assert_eq!(groups[0].detail, "/opt/homebrew/bin/mpv");
        assert!(!groups[0].is_user_facing());
    }

    #[test]
    fn sub_bundle_children_and_agents_join_their_app() {
        let app = "/Applications/Example.app";
        let procs = [
            proc(50, 500, 1, &format!("{app}/Contents/MacOS/Example"), Some("com.example.app"), false),
            // Started by the app from outside its bundle, with a sub-bundle ID.
            proc(51, 501, 500, "/Library/Application Support/Example/audiod", Some("com.example.app.audio"), true),
            // A launch agent with a sub-bundle ID and no process relation at all.
            proc(52, 502, 1, "/Users/me/Library/Example/agent", Some("com.example.app.agent"), false),
            // A child with an unrelated bundle ID stays apart.
            proc(53, 503, 500, "/usr/local/bin/tool", Some("org.other.tool"), false),
        ];
        let mut lookup = Fake::default();
        lookup.paths.insert(500, format!("{app}/Contents/MacOS/Example"));
        lookup.bundles.insert(app.into(), "com.example.app".into());
        let groups = group_processes(&procs, &me(), &mut lookup);
        let keys: Vec<&str> = groups.iter().map(|g| g.key.as_str()).collect();
        assert_eq!(keys, vec!["com.example.app", "org.other.tool"]);
        assert_eq!(groups[0].pids, vec![500, 501, 502]);
        assert!(groups[0].playing);
    }

    #[test]
    fn never_lists_sidestage() {
        let mut agent = proc(61, 601, 1, "/usr/libexec/agent", Some("com.apple.agent"), true);
        agent.responsible = 9999;
        let procs = [
            proc(60, 9999, 1, "/Users/me/dev/target/debug/sidestage", None, true),
            agent,
            proc(62, 602, 9999, "/usr/bin/afplay", None, true),
            proc(
                63,
                603,
                1,
                "/Applications/Sidestage.app/Contents/MacOS/sidestage",
                Some("com.pixityy.sidestage"),
                true,
            ),
        ];
        let groups = group_processes(&procs, &me(), &mut Fake::default());
        assert!(groups.is_empty(), "{groups:#?}");
    }

    #[test]
    fn key_falls_back_to_main_executable_then_path() {
        let procs = [
            proc(70, 700, 1, "/Applications/A.app/Contents/MacOS/A", Some("com.a"), false),
            proc(71, 701, 1, "/Applications/B.app/Contents/MacOS/B", None, false),
            proc(72, 702, 1, "/Applications/C.app/Contents/Helpers/h", Some("com.c.helper"), false),
        ];
        let groups = group_processes(&procs, &me(), &mut Fake::default());
        let keys: Vec<&str> = groups.iter().map(|g| g.key.as_str()).collect();
        assert_eq!(keys, vec!["com.a", "/Applications/B.app", "/Applications/C.app"]);
    }

    #[test]
    fn same_bundle_id_in_two_places_is_one_app() {
        let procs = [
            proc(80, 800, 1, "/Applications/Spotify.app/Contents/MacOS/Spotify", Some("com.spotify.client"), true),
            proc(
                81,
                801,
                1,
                "/Users/me/Applications/Spotify.app/Contents/MacOS/Spotify",
                Some("com.spotify.client"),
                false,
            ),
        ];
        let groups = group_processes(&procs, &me(), &mut Fake::default());
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].pids, vec![800, 801]);
    }
}
