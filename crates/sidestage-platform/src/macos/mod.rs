//! macOS backend: Core Audio process taps (macOS 14.2+).
//!
//! * **Capture and hide**: a private `CATapDescription` over every process object of the chosen
//!   app with `CATapMutedWhenTapped`. macOS silences the app's own output while Sidestage reads
//!   the tap and un-silences it the moment the tap is destroyed, also when Sidestage crashes
//!   (taps and private aggregates belong to the process that created them). Nothing outlives
//!   the process, so there is nothing to journal, guard or recover.
//! * **Output**: a private aggregate device whose main (clock) sub-device is the headphone
//!   output and whose tap list holds the tap. One IOProc reads the tap (the trailing input
//!   streams) and writes the rendered audio to the output streams in the same cycle: one clock,
//!   so no ring buffer, resampler or drift control.
//! * **Sidechain**: a second, unmuted global tap that excludes the music app and Sidestage, in
//!   a tap-only private aggregate whose IOProc only measures a peak.
//!
//! `group`, `plan`, `render`, `sys` and `worker` are pure logic, unit-tested without Core
//! Audio; `ca` wraps Core Audio, `procs` and `devices` enumerate apps and outputs, `engine`
//! assembles taps, aggregates and IOProcs, and `session` runs the control thread.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

mod group;
mod plan;
mod render;
mod sys;
mod worker;

#[cfg(target_os = "macos")]
mod ca;
#[cfg(target_os = "macos")]
mod devices;
#[cfg(target_os = "macos")]
mod engine;
#[cfg(target_os = "macos")]
mod procs;
#[cfg(target_os = "macos")]
mod session;

#[cfg(target_os = "macos")]
pub use platform::create;

/// No helper process on macOS: the OS itself un-mutes the app when Sidestage's tap dies.
#[cfg(target_os = "macos")]
pub fn run_helper(args: &[String]) -> i32 {
    log::warn!("unknown helper request on macOS: {args:?}");
    2
}

#[cfg(target_os = "macos")]
mod platform {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::{Duration, Instant};

    use anyhow::Context;
    use objc2::rc::autoreleasepool;

    use super::group::{AppGroup, SelfInfo};
    use super::procs::{self, SysLookup};
    use super::session::{MacSession, PROMPT_TIMED_OUT, Permission};
    use super::sys::OsVersion;
    use super::worker::{Answer, Worker};
    use super::{ca, devices};
    use crate::{
        AppKey, AudioApp, HideMethod, OutputDevice, Platform, ProcessorFactory, Session, SessionConfig, Support,
    };

    /// Apps that played this recently stay in the picker while paused.
    const RECENTLY_PLAYED: Duration = Duration::from_secs(15 * 60);
    /// Longest a picker query may hold up the UI thread; after that it gets the last answer.
    const QUERY_WAIT: Duration = Duration::from_millis(150);
    const FIRST_QUERY_WAIT: Duration = Duration::from_secs(2);

    const PERMISSION_NEEDED: &str = "Sidestage needs permission to capture other apps' audio. macOS asks when you \
        start; if you declined, allow Sidestage in System Settings > Privacy & Security > Screen & System Audio \
        Recording (\"System Audio Recording Only\").";

    pub fn create() -> anyhow::Result<Box<dyn Platform>> {
        if let Err(e) = ca::use_own_notification_thread() {
            log::warn!("Core Audio notifications stay on the main run loop: {e:#}");
        }
        let mut lookup = SysLookup::default();
        let me = autoreleasepool(|_| procs::self_info(&mut lookup));
        let os = procs::os_version();
        log::info!(
            "macOS {} (process taps need {})",
            os.map_or("unknown".to_string(), |v| v.to_string()),
            OsVersion::PROCESS_TAPS
        );
        Ok(Box::new(MacPlatform {
            os,
            permission: Arc::new(Permission::default()),
            me,
            lookup: Arc::new(Mutex::new(lookup)),
            worker: Worker::new("sidestage-coreaudio-query").context("starting the audio query thread")?,
            apps_answer: Answer::default(),
            outputs_answer: Answer::default(),
            recent: HashMap::new(),
            last_apps: None,
            last_outputs: None,
        }))
    }

    struct MacPlatform {
        os: Option<OsVersion>,
        permission: Arc<Permission>,
        me: SelfInfo,
        /// Bundle-ID cache; only the worker's queries use it.
        lookup: Arc<Mutex<SysLookup>>,
        /// Runs the picker's Core Audio queries off the UI thread.
        worker: Worker,
        apps_answer: Answer<anyhow::Result<Vec<AppGroup>>>,
        outputs_answer: Answer<anyhow::Result<Vec<OutputDevice>>>,
        recent: HashMap<String, Instant>,
        /// Last answers, for when a query is slow; `None` until the first one.
        last_apps: Option<Vec<AudioApp>>,
        last_outputs: Option<Vec<OutputDevice>>,
    }

    /// How long a picker query may take. The first one gets longer: its bundle lookups are
    /// cold, and a one-shot caller (the CLI) has no earlier answer to fall back on.
    fn query_wait<T>(previous: &Option<T>) -> Duration {
        if previous.is_some() { QUERY_WAIT } else { FIRST_QUERY_WAIT }
    }

    /// The last answer when Core Audio is slow, or an error when there is none.
    fn previous<T: Clone>(last: &Option<Vec<T>>, what: &str) -> anyhow::Result<Vec<T>> {
        match last {
            Some(list) => {
                log::debug!("Core Audio is slow to answer; showing the previous {what} list");
                Ok(list.clone())
            }
            None => anyhow::bail!("Core Audio did not answer in time; the {what} list is not available yet"),
        }
    }

    impl MacPlatform {
        fn unsupported(&self) -> Option<String> {
            self.os.filter(|v| *v < OsVersion::PROCESS_TAPS).map(|v| {
                format!(
                    "Sidestage needs macOS {} or newer to capture one app's audio; this Mac runs macOS {v}.",
                    OsVersion::PROCESS_TAPS
                )
            })
        }
    }

    impl Platform for MacPlatform {
        fn name(&self) -> &'static str {
            "macOS · Core Audio process taps"
        }

        fn support(&self) -> Support {
            if let Some(why) = self.unsupported() {
                Support::Unsupported(why)
            } else if self.permission.denied() {
                Support::NeedsPermission(PERMISSION_NEEDED.to_string())
            } else if self.permission.timed_out() {
                Support::NeedsPermission(PROMPT_TIMED_OUT.to_string())
            } else {
                Support::Supported
            }
        }

        fn list_apps(&mut self) -> anyhow::Result<Vec<AudioApp>> {
            let me = self.me.clone();
            let lookup = self.lookup.clone();
            let query = move || {
                let mut lookup = lookup.lock().unwrap_or_else(PoisonError::into_inner);
                autoreleasepool(|_| procs::app_groups(&me, &mut lookup))
            };
            let wait = query_wait(&self.last_apps);
            let Some(groups) = self.worker.run(&self.apps_answer, wait, query) else {
                return previous(&self.last_apps, "app");
            };
            let groups = groups?;
            let now = Instant::now();
            for g in groups.iter().filter(|g| g.playing) {
                self.recent.insert(g.key.clone(), now);
            }
            self.recent.retain(|_, seen| now.duration_since(*seen) < RECENTLY_PLAYED);
            let apps: Vec<AudioApp> = groups
                .into_iter()
                .filter(|g| g.playing || g.is_user_facing() || self.recent.contains_key(&g.key))
                .map(|g| AudioApp {
                    key: AppKey(g.key),
                    name: g.name,
                    detail: g.detail,
                    pids: g.pids,
                    playing: g.playing,
                    peak: None,
                })
                .collect();
            self.last_apps = Some(apps.clone());
            Ok(apps)
        }

        fn list_outputs(&mut self) -> anyhow::Result<Vec<OutputDevice>> {
            let query = || autoreleasepool(|_| devices::list_outputs());
            let wait = query_wait(&self.last_outputs);
            let Some(outputs) = self.worker.run(&self.outputs_answer, wait, query) else {
                return previous(&self.last_outputs, "output");
            };
            let outputs = outputs?;
            self.last_outputs = Some(outputs.clone());
            Ok(outputs)
        }

        fn hide_methods(&mut self) -> Vec<(HideMethod, String)> {
            vec![
                (HideMethod::Auto, "Mute the original while Sidestage plays it (recommended)".to_string()),
                (HideMethod::KeepOriginal, "Keep the original audible too (for comparison)".to_string()),
            ]
        }

        fn start(&mut self, config: SessionConfig, processor: ProcessorFactory) -> anyhow::Result<Box<dyn Session>> {
            if let Some(why) = self.unsupported() {
                anyhow::bail!(why);
            }
            if config.app.0.is_empty() {
                anyhow::bail!("no app selected");
            }
            log::info!(
                "starting macOS session for {} (output {:?}, hide {:?}, sidechain {})",
                config.app.0,
                config.output,
                config.hide,
                config.sidechain
            );
            let session = MacSession::start(config, processor, self.permission.clone(), self.me.clone(), self.os)?;
            Ok(Box::new(session))
        }

        fn recover(&mut self) -> anyhow::Result<Vec<String>> {
            // Taps and private aggregates die with the process that created them, and with
            // them the muting: nothing can be left behind.
            Ok(Vec::new())
        }
    }
}
