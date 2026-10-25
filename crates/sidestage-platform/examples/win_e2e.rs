//! Live end-to-end test of the Windows backend.
//!
//! Safe by design: the only processes whose volume, mute or routing this test changes (directly
//! or through Sidestage) are the quiet test tones it starts itself (copies of `win_tone`, at
//! most 0.01). Every other app is left alone: the sidechain checks put the apps that were
//! already playing on the ignore list, so they are not even listened to. The tones' volumes are
//! put back even when a check fails. It refuses to run unless `SIDESTAGE_STATE_DIR` names a
//! folder of its own, so its journal, guardians and their locks never meet those of a Sidestage
//! the user is running.
//!
//! What it checks (one source, as in v0.1):
//! 1. `list_apps` finds the tone, and how long it takes.
//! 2. KeepOriginal: the capture matches the tone and its volume is untouched.
//! 3. Hide (Auto): the tone's session sits at ~1e-6 while the captured level still matches the
//!    tone within 1 dB (makeup gain), and `stop()` restores the volume. Dropping the session
//!    without `stop()` restores it too.
//! 4. A tone that opens its stream after Sidestage attached is "waiting", then hidden.
//! 5. A foreign volume change (as if the user moved the mixer slider) is adopted and re-hushed;
//!    a slider drag is not a fight, an app that resets its volume is; killing and restarting
//!    the tone re-attaches; `stop()` restores the adopted volume.
//! 6. Another instance of the app is left alone, except that a session of it that Windows
//!    started at the hushed volume gets the app's volume back (a deliberate 0 stays 0).
//! 7. Crash: a child process runs a hiding session and is killed together with its whole
//!    process tree; the guardian, which is not in that tree, restores the volume. `recover()`
//!    then finds nothing left. The same while the app is closed: `recover()` keeps the entry
//!    and the volume comes back the moment the app plays; without `recover()`, the dead run's
//!    guardian does it.
//!
//! Several sources:
//! 8. Problems stay with their source: Sidestage itself as a source, a processor that cannot be
//!    built and one that panics while playing each fail only that source (its original stays
//!    or becomes audible) while the others play on. Invalid lists are refused.
//! 9. Three tones (different apps, frequencies and levels) with pass-through processors of
//!    different gains: each is captured at its own level and the master gets the right mix.
//! 10. `set_sources` adds a fourth source and removes one while the untouched sources' session
//!     volumes are watched every few ms (they must stay hushed) and their audio is checked for
//!     gaps; their processors are not rebuilt. Duplicate apps or IDs are refused.
//! 11. A 40 s soak with three sources: no dropouts. A source removed while its app is closed
//!     stays journaled and is restored the moment the app plays again.
//! 12. Crash with three hidden sources: the guardian restores all three. Then a child session
//!     that keeps adding and removing sources is killed at many different moments (journal
//!     writes, hushes and restores in flight): every time, every tone comes back.
//! 13. Sidechain: a game tone (not a source) counts at its level, an ignored tone does not,
//!     sources do not (also one that keeps its original audible), Sidestage's own output does
//!     not, and changing the ignore list takes effect while the session runs. The picker still
//!     trusts the meter of an app the sidechain listens to, and how long a sound the game
//!     starts takes to reach the processors is measured.
//! 14. The output stream dies while three sources play (as on a device change): it is reopened,
//!     every source is rebuilt, and no app is unhidden meanwhile.
//!
//! ```text
//! cargo build -p sidestage-platform --examples
//! set SIDESTAGE_STATE_DIR=%TEMP%\sidestage-e2e
//! target\debug\examples\win_e2e.exe            # all checks
//! target\debug\examples\win_e2e.exe --multi    # only the multi-source checks (8-14)
//! target\debug\examples\win_e2e.exe --soak 120 # three sources for 120 s, report drift/dropouts
//! target\debug\examples\win_e2e.exe --soak 120 --default-output # same, output on another clock
//! target\debug\examples\win_e2e.exe --sidechain # sidechain checks only
//! target\debug\examples\win_e2e.exe --churn 20 # kill a session 20 times while its sources change
//! target\debug\examples\win_e2e.exe --reopen   # the output dies and is reopened (check 14)
//! target\debug\examples\win_e2e.exe --ramp 5   # how long a session-volume change takes to
//!                                              # reach the capture (5 rounds)
//! ```

#[cfg(windows)]
fn main() {
    if let Some(code) = sidestage_platform::run_helper_if_requested() {
        std::process::exit(code);
    }
    log::set_logger(&e2e::LOGGER).ok();
    log::set_max_level(log::LevelFilter::Info);
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !matches!(args.first().map(String::as_str), Some("--child-session" | "--child-churn"))
        && let Err(e) = e2e::check_state_dir()
    {
        eprintln!("FAIL: {e:#}");
        std::process::exit(2);
    }
    let code = match args.first().map(String::as_str) {
        Some("--child-session") => e2e::child_session(&args[1..]),
        Some("--child-churn") => e2e::child_churn(&args[1..]),
        Some("--churn") => e2e::churn_only(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(12)),
        Some("--reopen") => e2e::reopen_only(),
        Some("--sidechain") => e2e::sidechain_only(),
        Some("--soak") => e2e::soak_only(
            args.get(1).and_then(|s| s.parse().ok()).unwrap_or(60),
            args.iter().any(|a| a == "--default-output"),
        ),
        Some("--ramp") => e2e::ramp(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(5)),
        Some("--multi") => e2e::run_with(true),
        _ => e2e::run_with(false),
    };
    std::process::exit(code);
}

#[cfg(not(windows))]
fn main() {
    eprintln!("win_e2e is a Windows-only test");
}

#[cfg(windows)]
mod e2e {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::{Duration, Instant};

    use anyhow::{Context, anyhow, bail, ensure};
    use sidestage_platform::{
        AppKey, AtomicLevel, AudioApp, HideMethod, Platform, Processor, ProcessorFactory, Session, SessionConfig,
        SessionState, SessionStatus, SourceConfig, SourceId, SourceStatus,
    };
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Media::Audio::{
        DEVICE_STATE_ACTIVE, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator, ISimpleAudioVolume,
        MMDeviceEnumerator, eRender,
    };
    use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
    use windows::Win32::System::Threading::{TerminateProcess, WaitForSingleObject};
    use windows::core::Interface;

    pub struct Logger;
    pub static LOGGER: Logger = Logger;
    static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

    impl log::Log for Logger {
        fn enabled(&self, m: &log::Metadata) -> bool {
            m.level() <= log::Level::Info
        }
        fn log(&self, r: &log::Record) {
            let t = T0.get_or_init(Instant::now).elapsed().as_secs_f32();
            eprintln!("  [{t:7.3}] {:5} {}", r.level(), r.args());
        }
        fn flush(&self) {}
    }

    /// The test must never share the journal of a Sidestage the user is running.
    pub fn check_state_dir() -> anyhow::Result<()> {
        let dir = std::env::var_os(sidestage_platform::journal::STATE_DIR_ENV)
            .filter(|d| !d.is_empty())
            .context("set SIDESTAGE_STATE_DIR to a folder of the test's own before running it")?;
        let real = std::env::var_os("LOCALAPPDATA").map(|d| std::path::Path::new(&d).join("Sidestage"));
        ensure!(
            real.is_none_or(|r| !std::path::Path::new(&dir).starts_with(&r)),
            "SIDESTAGE_STATE_DIR must not be Sidestage's real state folder"
        );
        println!("  state dir: {}", std::path::Path::new(&dir).display());
        Ok(())
    }

    // ---- test tones --------------------------------------------------------------------------

    /// A renamed copy of `win_tone.exe` (each name is a separate app with its own volume).
    fn tone_exe(name: &str) -> anyhow::Result<std::path::PathBuf> {
        let src = std::env::current_exe()?.with_file_name("win_tone.exe");
        ensure!(src.exists(), "{} not found; run `cargo build -p sidestage-platform --examples`", src.display());
        if name == "win_tone" {
            return Ok(src);
        }
        let exe = src.with_file_name(format!("{name}.exe"));
        // A copy left running by an aborted run keeps the file locked; reuse it then.
        if let Err(e) = std::fs::copy(&src, &exe) {
            ensure!(exe.exists(), "copying {}: {e}", src.display());
        }
        Ok(exe)
    }

    /// A tone started as a child of this process.
    struct Tone {
        child: Child,
        pid: u32,
        amp: f32,
        freq: f32,
        path: String,
    }

    impl Tone {
        fn start() -> anyhow::Result<Tone> {
            Self::start_with(&[])
        }

        fn start_with(extra: &[&str]) -> anyhow::Result<Tone> {
            Self::named("win_tone", 997.0, 0.01, extra)
        }

        /// `amp` is capped at 0.01 by `win_tone` (0.001 without a virtual output).
        fn named(name: &str, freq: f32, amp: f32, extra: &[&str]) -> anyhow::Result<Tone> {
            assert!(amp <= 0.01, "test tones must stay quiet");
            let exe = tone_exe(name)?;
            let mut child = Command::new(&exe)
                .args(["--seconds", "900", "--freq", &freq.to_string(), "--amp", &amp.to_string()])
                .args(extra)
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .with_context(|| format!("starting {name}"))?;
            let mut line = String::new();
            BufReader::new(child.stdout.take().context("tone stdout")?).read_line(&mut line)?;
            let field = |k: &str| {
                line.split_whitespace().find_map(|w| w.strip_prefix(k)).map(str::to_owned).unwrap_or_default()
            };
            let amp: f32 = field("amp=").parse().context("tone amplitude")?;
            let device = line.split("device=").nth(1).unwrap_or("?").trim().to_owned();
            ensure!(amp <= 0.01, "tone too loud");
            println!("  tone {name}: pid {} amp {amp} {freq} Hz on '{device}'", child.id());
            Ok(Tone { pid: child.id(), child, amp, freq, path: exe.to_string_lossy().into_owned() })
        }

        fn key(&self) -> AppKey {
            AppKey(self.path.to_lowercase())
        }

        /// RMS of the tone as captured (both channels carry the sine).
        fn rms(&self) -> f32 {
            self.amp / std::f32::consts::SQRT_2
        }

        fn kill(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    impl Drop for Tone {
        fn drop(&mut self) {
            self.kill();
        }
    }

    /// A tone in a process tree of its own (started as a child of the shell), laid out like a
    /// real game: outside Sidestage's tree, so the sidechain hears it unless it is a source or
    /// ignored. It runs a renamed copy of `win_tone.exe`, so its volume is separate from the
    /// other tones'.
    struct OutsideTone {
        process: HANDLE,
        pid: u32,
        path: String,
    }

    impl OutsideTone {
        fn start(name: &str, seconds: u64, amp: f32) -> anyhow::Result<OutsideTone> {
            Self::start_with(name, seconds, amp, "")
        }

        /// `extra` is appended to `win_tone`'s command line.
        fn start_with(name: &str, seconds: u64, amp: f32, extra: &str) -> anyhow::Result<OutsideTone> {
            use windows::Win32::System::Threading::{
                CREATE_NO_WINDOW, CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
                InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_PARENT_PROCESS,
                PROCESS_INFORMATION, STARTUPINFOEXW, STARTUPINFOW, UpdateProcThreadAttribute,
            };
            assert!(amp <= 0.01, "test tones must stay quiet");
            let exe = tone_exe(name)?;
            let shell = find_shell()?;
            let mut size = 0usize;
            // The first call only reports the size (and fails with ERROR_INSUFFICIENT_BUFFER).
            let _ = unsafe { InitializeProcThreadAttributeList(None, 1, None, &mut size) };
            let mut buf = vec![0u8; size];
            let list = LPPROC_THREAD_ATTRIBUTE_LIST(buf.as_mut_ptr().cast());
            unsafe { InitializeProcThreadAttributeList(Some(list), 1, None, &mut size) }?;
            let parent = shell.raw();
            let mut pi = PROCESS_INFORMATION::default();
            let created = (|| -> anyhow::Result<()> {
                unsafe {
                    UpdateProcThreadAttribute(
                        list,
                        0,
                        PROC_THREAD_ATTRIBUTE_PARENT_PROCESS as usize,
                        Some((&raw const parent).cast()),
                        size_of::<HANDLE>(),
                        None,
                        None,
                    )
                }?;
                let si = STARTUPINFOEXW {
                    StartupInfo: STARTUPINFOW { cb: size_of::<STARTUPINFOEXW>() as u32, ..Default::default() },
                    lpAttributeList: list,
                };
                let mut cmd: Vec<u16> =
                    format!("\"{}\" --seconds {seconds} --freq 700 --amp {amp} {extra}", exe.display())
                        .encode_utf16()
                        .chain([0])
                        .collect();
                unsafe {
                    CreateProcessW(
                        None,
                        Some(windows::core::PWSTR(cmd.as_mut_ptr())),
                        None,
                        None,
                        false,
                        EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW,
                        None,
                        None,
                        &si.StartupInfo,
                        &mut pi,
                    )
                }
                .context("starting a tone outside this process tree")?;
                Ok(())
            })();
            unsafe { DeleteProcThreadAttributeList(list) };
            drop(shell);
            created?;
            unsafe {
                let _ = CloseHandle(pi.hThread);
            }
            println!("  outside tone '{name}': pid {} amp {amp}", pi.dwProcessId);
            Ok(OutsideTone { process: pi.hProcess, pid: pi.dwProcessId, path: exe.to_string_lossy().into_owned() })
        }

        fn key(&self) -> AppKey {
            AppKey(self.path.to_lowercase())
        }
    }

    impl Drop for OutsideTone {
        fn drop(&mut self) {
            unsafe {
                let _ = TerminateProcess(self.process, 0);
                WaitForSingleObject(self.process, 2000);
                let _ = CloseHandle(self.process);
            }
        }
    }

    /// A handle to the user's shell (explorer.exe) that new processes can be parented to.
    fn find_shell() -> anyhow::Result<OwnedHandle> {
        use windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
        };
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_CREATE_PROCESS};
        let snap = OwnedHandle(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }?);
        let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = unsafe { Process32FirstW(snap.raw(), &mut e) }.is_ok();
        while ok {
            let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
            if String::from_utf16_lossy(&e.szExeFile[..n]).eq_ignore_ascii_case("explorer.exe")
                && let Ok(h) = unsafe { OpenProcess(PROCESS_CREATE_PROCESS, false, e.th32ProcessID) }
            {
                return Ok(OwnedHandle(h));
            }
            ok = unsafe { Process32NextW(snap.raw(), &mut e) }.is_ok();
        }
        bail!("no explorer.exe to start a separate process tree from")
    }

    struct OwnedHandle(HANDLE);

    impl OwnedHandle {
        fn raw(&self) -> HANDLE {
            self.0
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    // ---- reading and resetting the tones' session volumes (only ever the tones') -------------

    fn enumerator() -> anyhow::Result<IMMDeviceEnumerator> {
        Ok(unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }?)
    }

    /// Volume controls of every session that belongs to `pid`.
    fn sessions_of(pid: u32) -> anyhow::Result<Vec<ISimpleAudioVolume>> {
        let en = enumerator()?;
        let devs = unsafe { en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }?;
        let mut out = Vec::new();
        for i in 0..unsafe { devs.GetCount() }? {
            let dev = unsafe { devs.Item(i) }?;
            let Ok(mgr) = (unsafe { dev.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) }) else {
                continue;
            };
            let se = unsafe { mgr.GetSessionEnumerator() }?;
            for j in 0..unsafe { se.GetCount() }? {
                let ctl = unsafe { se.GetSession(j) }?;
                let c2: IAudioSessionControl2 = ctl.cast()?;
                if unsafe { c2.GetProcessId() }.ok() == Some(pid) {
                    out.push(ctl.cast()?);
                }
            }
        }
        Ok(out)
    }

    fn volume_of(pid: u32) -> anyhow::Result<(f32, bool)> {
        let s = sessions_of(pid)?;
        let v = s.first().ok_or_else(|| anyhow!("the tone has no audio session"))?;
        Ok(unsafe { (v.GetMasterVolume()?, v.GetMute()?.as_bool()) })
    }

    fn set_volume_of(pid: u32, volume: f32, mute: bool) -> anyhow::Result<()> {
        for v in sessions_of(pid)? {
            unsafe {
                v.SetMasterVolume(volume, std::ptr::null())?;
                v.SetMute(mute, std::ptr::null())?;
            }
        }
        Ok(())
    }

    /// Waits until Sidestage has turned the tone back down.
    fn wait_hushed(pid: u32, secs: f32) -> anyhow::Result<()> {
        let t = Instant::now();
        while t.elapsed().as_secs_f32() < secs {
            if volume_of(pid)?.0 < 1e-5 {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        bail!("the tone was not hushed again")
    }

    fn wait_for_session(pid: u32) -> anyhow::Result<()> {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(10) {
            if !sessions_of(pid)?.is_empty() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        bail!("the tone's audio session did not appear")
    }

    /// Waits until the tone's volume is back at `want`; returns how long that took.
    fn wait_volume(pid: u32, want: f32, secs: f32) -> anyhow::Result<Duration> {
        let t = Instant::now();
        while t.elapsed().as_secs_f32() < secs {
            if (volume_of(pid)?.0 - want).abs() < 1e-4 {
                return Ok(t.elapsed());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        bail!("pid {pid}: the volume did not come back to {want} within {secs} s (now {:?})", volume_of(pid).ok())
    }

    /// Makes a tone's saved volume usable (an aborted run may have left it hushed) and returns
    /// it, to be put back at the end.
    fn usable_volume(pid: u32) -> anyhow::Result<(f32, bool)> {
        wait_for_session(pid)?;
        let (v, m) = volume_of(pid)?;
        if v < 0.01 || m {
            println!("  pid {pid}: saved volume {v:e} muted={m} (left over from an aborted run); resetting it to 1.0");
            set_volume_of(pid, 1.0, false)?;
            return Ok((1.0, false));
        }
        Ok((v, m))
    }

    /// Lowest and highest session volume a watched tone had (-1 = muted), and how often it was
    /// read.
    type Watched = Vec<(f32, f32, u64)>;

    /// Watches tones' session volumes from a thread of its own, every few milliseconds.
    struct VolumeWatch {
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<anyhow::Result<Watched>>>,
    }

    impl VolumeWatch {
        fn start(pids: Vec<u32>) -> VolumeWatch {
            let stop = Arc::new(AtomicBool::new(false));
            let s = stop.clone();
            let thread = std::thread::spawn(move || {
                unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
                let vols: Vec<Vec<ISimpleAudioVolume>> =
                    pids.iter().map(|&p| sessions_of(p)).collect::<anyhow::Result<_>>()?;
                // (min, max, readings) per tone.
                let mut out = vec![(f32::MAX, 0.0f32, 0u64); pids.len()];
                while !s.load(Ordering::Relaxed) {
                    for (i, vs) in vols.iter().enumerate() {
                        for v in vs {
                            let x = unsafe { v.GetMasterVolume() }?;
                            let muted = unsafe { v.GetMute() }?.as_bool();
                            let x = if muted { -1.0 } else { x };
                            out[i] = (out[i].0.min(x), out[i].1.max(x), out[i].2 + 1);
                        }
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                Ok(out)
            });
            VolumeWatch { stop, thread: Some(thread) }
        }

        fn finish(mut self) -> anyhow::Result<Watched> {
            self.stop.store(true, Ordering::Relaxed);
            let t = self.thread.take().context("watch already finished")?;
            t.join().map_err(|_| anyhow!("the volume watch panicked"))?
        }
    }

    // ---- processors that measure -------------------------------------------------------------

    /// What a test processor saw, readable from the test thread.
    #[derive(Default)]
    struct Meter {
        sumsq: AtomicU64,
        frames: AtomicU64,
        blocks: AtomicU64,
        /// Blocks of pure silence after the first sound (gaps in the source's audio).
        gaps: AtomicU64,
        heard: AtomicBool,
        /// Loudest sidechain level the processor was handed.
        sidechain: AtomicLevel,
        /// Processors the factory built.
        built: AtomicU64,
    }

    /// A source processor: passes its input through at `gain` and measures it. With `emit`, it
    /// writes a 440 Hz tone instead (only ever on the virtual output, at most 0.01).
    struct Probe {
        meter: Arc<Meter>,
        gain: f32,
        sumsq: f64,
        frames: u64,
        emit: f32,
        phase: f32,
        step: f32,
    }

    impl Processor for Probe {
        fn process(&mut self, input: &[f32], output: &mut [f32], sidechain_peak: f32) {
            if self.emit > 0.0 {
                for f in output.as_chunks_mut::<2>().0 {
                    let s = (self.phase * std::f32::consts::TAU).sin() * self.emit;
                    self.phase = (self.phase + self.step).fract();
                    *f = [s, s];
                }
            } else {
                for (o, i) in output.iter_mut().zip(input) {
                    *o = i * self.gain;
                }
            }
            self.meter.sidechain.raise(sidechain_peak);
            if input.iter().all(|&x| x == 0.0) {
                if self.meter.heard.load(Ordering::Relaxed) {
                    self.meter.gaps.fetch_add(1, Ordering::Relaxed);
                }
            } else {
                self.meter.heard.store(true, Ordering::Relaxed);
            }
            for f in input.as_chunks::<2>().0 {
                self.sumsq += 0.5 * (f64::from(f[0]) * f64::from(f[0]) + f64::from(f[1]) * f64::from(f[1]));
            }
            self.frames += (input.len() / 2) as u64;
            self.meter.sumsq.store(self.sumsq.to_bits(), Ordering::Relaxed);
            self.meter.frames.store(self.frames, Ordering::Relaxed);
            self.meter.blocks.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl Meter {
        fn snapshot(&self) -> (f64, u64) {
            (f64::from_bits(self.sumsq.load(Ordering::Relaxed)), self.frames.load(Ordering::Relaxed))
        }

        /// RMS of the processor input over `secs`.
        fn rms_over(&self, secs: f32) -> f32 {
            let (s0, n0) = self.snapshot();
            std::thread::sleep(Duration::from_secs_f32(secs));
            let (s1, n1) = self.snapshot();
            if n1 <= n0 { 0.0 } else { (((s1 - s0) / (n1 - n0) as f64).max(0.0)).sqrt() as f32 }
        }
    }

    /// A processor that measures and outputs silence.
    fn factory(meter: &Arc<Meter>) -> ProcessorFactory {
        probe_factory(meter, 0.0, 0.0)
    }

    fn probe_factory(meter: &Arc<Meter>, gain: f32, emit: f32) -> ProcessorFactory {
        assert!(emit <= 0.01 && gain <= 1.0, "test output must stay quiet");
        let meter = meter.clone();
        Arc::new(move |rate| {
            meter.built.fetch_add(1, Ordering::Relaxed);
            Box::new(Probe {
                meter: meter.clone(),
                gain,
                sumsq: 0.0,
                frames: 0,
                emit,
                phase: 0.0,
                step: 440.0 / rate as f32,
            }) as Box<dyn Processor>
        })
    }

    /// The master: hands the mix (left channel) to the test through a lock-free ring and
    /// outputs silence, or `emit` (a 440 Hz tone, at most 0.01) to check that Sidestage's own
    /// output never reaches the sidechain.
    struct Mix {
        tx: rtrb::Producer<f32>,
        rate: f32,
        emit: f32,
        phase: f32,
    }

    impl Processor for Mix {
        fn process(&mut self, input: &[f32], output: &mut [f32], _sidechain_peak: f32) {
            for f in input.as_chunks::<2>().0 {
                // Full while the test is not measuring: those samples are not needed.
                let _ = self.tx.push(f[0]);
            }
            if self.emit > 0.0 {
                let step = 440.0 / self.rate;
                for f in output.as_chunks_mut::<2>().0 {
                    let s = (self.phase * std::f32::consts::TAU).sin() * self.emit;
                    self.phase = (self.phase + step).fract();
                    *f = [s, s];
                }
            } else {
                output.fill(0.0);
            }
        }
    }

    /// The test's end of the master's ring, with the master's sample rate.
    type MixRing = Arc<Mutex<Option<(rtrb::Consumer<f32>, u32)>>>;

    /// Measures the amplitude of each test frequency in the mix.
    #[derive(Clone)]
    struct MixMeter {
        freqs: Vec<f32>,
        ring: MixRing,
    }

    impl MixMeter {
        fn new(freqs: &[f32]) -> MixMeter {
            MixMeter { freqs: freqs.to_vec(), ring: Arc::default() }
        }

        fn factory(&self, emit: f32) -> ProcessorFactory {
            assert!(emit <= 0.01, "test output must stay quiet");
            let ring = self.ring.clone();
            Arc::new(move |rate| {
                let (tx, rx) = rtrb::RingBuffer::new(rate as usize * 4);
                *ring.lock().unwrap_or_else(|p| p.into_inner()) = Some((rx, rate));
                Box::new(Mix { tx, rate: rate as f32, emit, phase: 0.0 }) as Box<dyn Processor>
            })
        }

        /// Amplitude of each frequency in the mix over the next `secs`. The bridges' speed
        /// correction (up to 1000 ppm while they settle) moves a tone slightly, so each
        /// frequency is searched within +-1000 ppm.
        fn amplitudes(&self, secs: f32) -> Vec<f32> {
            let take = |all: bool| -> (Vec<f32>, u32) {
                let mut g = self.ring.lock().unwrap_or_else(|p| p.into_inner());
                let Some((rx, rate)) = g.as_mut() else { return (Vec::new(), 48_000) };
                let mut out = Vec::new();
                while let Ok(x) = rx.pop() {
                    if all {
                        out.push(x);
                    }
                }
                (out, *rate)
            };
            take(false);
            std::thread::sleep(Duration::from_secs_f32(secs));
            let (x, rate) = take(true);
            let n = x.len().max(1) as f64;
            let dft = |hz: f64| {
                let w = std::f64::consts::TAU * hz / f64::from(rate);
                let (mut c, mut s) = (0.0, 0.0);
                for (i, &v) in x.iter().enumerate() {
                    c += f64::from(v) * (w * i as f64).cos();
                    s += f64::from(v) * (w * i as f64).sin();
                }
                (2.0 * (c * c + s * s).sqrt() / n) as f32
            };
            self.freqs
                .iter()
                .map(|&f| (-20..=20).map(|k| dft(f64::from(f) * (1.0 + f64::from(k) * 50e-6))).fold(0.0, f32::max))
                .collect()
        }
    }

    /// A master that passes nothing through (most single-source checks).
    fn silent_master() -> ProcessorFactory {
        Arc::new(|_| {
            struct Quiet;
            impl Processor for Quiet {
                fn process(&mut self, _input: &[f32], output: &mut [f32], _sidechain_peak: f32) {
                    output.fill(0.0);
                }
            }
            Box::new(Quiet) as Box<dyn Processor>
        })
    }

    fn db(x: f32) -> f32 {
        20.0 * x.max(1e-12).log10()
    }

    // ---- sessions and their status ----------------------------------------------------------

    fn source(id: SourceId, app: AppKey, hide: HideMethod, processor: ProcessorFactory) -> SourceConfig {
        SourceConfig { id, app, hide, processor }
    }

    fn src(st: &SessionStatus, id: SourceId) -> anyhow::Result<&SourceStatus> {
        st.sources.iter().find(|s| s.id == id).with_context(|| format!("source {id} is not in the status"))
    }

    fn running_hidden(s: &SourceStatus) -> bool {
        s.state == SessionState::Running && s.original_hidden
    }

    /// Waits until every listed source satisfies `want`.
    fn wait_sources(
        s: &dyn Session,
        ids: &[SourceId],
        want: impl Fn(&SourceStatus) -> bool,
        secs: f32,
    ) -> anyhow::Result<f32> {
        let t = Instant::now();
        loop {
            let st = s.status();
            if let SessionState::Error(e) = &st.state {
                bail!("session error: {e}");
            }
            let mut all = true;
            for &id in ids {
                let x = src(&st, id)?;
                if let SessionState::Error(e) = &x.state {
                    bail!("source {id} error: {e}");
                }
                all &= want(x);
            }
            if all {
                return Ok(t.elapsed().as_secs_f32());
            }
            if t.elapsed().as_secs_f32() > secs {
                let states: Vec<String> = st
                    .sources
                    .iter()
                    .map(|x| format!("{}: {:?} hidden={} {:?}", x.id, x.state, x.original_hidden, x.warnings))
                    .collect();
                bail!("timed out; sources {states:?}, session warnings {:?}", st.warnings);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_hidden(s: &dyn Session, ids: &[SourceId], secs: f32) -> anyhow::Result<f32> {
        wait_sources(s, ids, running_hidden, secs)
    }

    fn print_status(s: &dyn Session) {
        let st = s.status();
        println!(
            "  status: {:?} out='{}' {} Hz output latency={:.1} ms output dropouts={} sidechain active={} peak={:.4}",
            st.state,
            st.output_name,
            st.sample_rate,
            st.latency_ms,
            st.dropouts,
            st.sidechain_active,
            st.sidechain_peak
        );
        for w in &st.warnings {
            println!("  warning: {w}");
        }
        for x in &st.sources {
            println!(
                "    source {} '{}': {:?} hidden={} latency={:.1} ms drift={:+.1} ppm dropouts={} capture_peak={:.5}",
                x.id, x.app_name, x.state, x.original_hidden, x.latency_ms, x.drift_ppm, x.dropouts, x.capture_peak
            );
            for w in &x.warnings {
                println!("      warning: {w}");
            }
        }
    }

    struct Ctx {
        platform: Box<dyn Platform>,
        key: AppKey,
        output: Option<String>,
        tone: Tone,
        /// The tone's volume and mute before the test; restored at the end no matter what.
        original: (f32, bool),
        /// Apps that had audio before the test (the user's): ignored by the sidechain checks.
        others: Vec<AppKey>,
    }

    impl Ctx {
        /// One source, the tone, with a measuring processor.
        fn single(&self, hide: HideMethod, meter: &Arc<Meter>) -> SessionConfig {
            SessionConfig {
                output: self.output.clone(),
                sidechain: true,
                sidechain_ignore: self.others.clone(),
                sources: vec![source(1, self.key.clone(), hide, factory(meter))],
                master: silent_master(),
            }
        }
    }

    /// All checks, or (`multi_only`) the multi-source ones.
    pub fn run_with(multi_only: bool) -> i32 {
        if let Err(e) = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok() {
            eprintln!("COM: {e}");
            return 1;
        }
        let tone = match Tone::start() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("FAIL: {e:#}");
                return 1;
            }
        };
        let pid = tone.pid;
        let mut original = None;
        let result = (|| -> anyhow::Result<()> {
            let (v, m) = usable_volume(pid)?;
            println!("  the tone has {} audio session(s)", sessions_of(pid)?.len());
            original = Some((v, m));
            let mut ctx = setup(tone, (v, m))?;
            let single = if multi_only { Ok(()) } else { single_source_checks(&mut ctx) };
            let r = single.and_then(|()| multi_source_checks(&mut ctx));
            // Always put the tone back the way we found it.
            original = None;
            let (ov, om) = ctx.original;
            let restore = sessions_of(ctx.tone.pid).and_then(|_| set_volume_of(ctx.tone.pid, ov, om));
            r.and(restore)
        })();
        if let Some((v, m)) = original {
            let _ = set_volume_of(pid, v, m);
        }
        match result {
            Ok(()) => {
                println!("\nPASS: all Windows backend checks passed");
                0
            }
            Err(e) => {
                println!("\nFAIL: {e:#}");
                1
            }
        }
    }

    /// The output the test plays to: the virtual device nobody listens to, if there is one.
    fn test_output(platform: &mut dyn Platform) -> anyhow::Result<Option<String>> {
        Ok(platform.list_outputs()?.into_iter().find(|o| o.name.contains("Steam Streaming Speakers")).map(|o| o.id))
    }

    /// Apps that are playing (or have audio sessions) apart from the test's own tones.
    fn other_apps(platform: &mut dyn Platform) -> anyhow::Result<Vec<AppKey>> {
        Ok(platform.list_apps()?.into_iter().filter(|a| !a.key.0.contains("win_tone")).map(|a| a.key).collect())
    }

    fn setup(tone: Tone, original: (f32, bool)) -> anyhow::Result<Ctx> {
        let mut platform = sidestage_platform::platform()?;
        println!("== platform: {} / {:?}", platform.name(), platform.support());
        let lines = platform.recover()?;
        println!("  recover() at start: {lines:?}");

        println!("== list_outputs");
        let outputs = platform.list_outputs()?;
        for o in &outputs {
            println!("  {}{} [{}]", if o.is_default { "* " } else { "  " }, o.name, o.id);
        }
        ensure!(outputs.iter().filter(|o| o.is_default).count() == 1, "exactly one default output");
        let output = test_output(platform.as_mut())?;

        println!("== list_apps");
        let key = tone.key();
        let mut times = Vec::new();
        let mut found: Option<AudioApp> = None;
        for _ in 0..20 {
            let t = Instant::now();
            let apps = platform.list_apps()?;
            times.push(t.elapsed().as_secs_f64() * 1000.0);
            found = apps.into_iter().find(|a| a.key == key);
            std::thread::sleep(Duration::from_millis(30));
        }
        let apps = platform.list_apps()?;
        for a in &apps {
            println!("  {:<28} {:<24} playing={} peak={:?} pids={:?}", a.name, a.detail, a.playing, a.peak, a.pids);
        }
        times.sort_by(f64::total_cmp);
        println!(
            "  list_apps over 20 calls: min {:.1} ms, median {:.1} ms, max {:.1} ms (first call included)",
            times[0],
            times[times.len() / 2],
            times[times.len() - 1]
        );
        let app = found.context("list_apps did not list the test tone")?;
        ensure!(app.pids.contains(&tone.pid), "the tone's pid is in the app");
        ensure!(times[times.len() - 1] < 200.0, "list_apps must take < 200 ms");
        ensure!(!apps.iter().any(|a| a.pids.contains(&std::process::id())), "Sidestage itself is not listed");
        let others = other_apps(platform.as_mut())?;
        println!("  the sidechain checks ignore the {} other app(s) with audio", others.len());
        Ok(Ctx { platform, key, output, tone, original, others })
    }

    fn single_source_checks(ctx: &mut Ctx) -> anyhow::Result<()> {
        let expect = ctx.tone.rms();
        let (orig_v, _) = ctx.original;

        // -- 2. KeepOriginal ---------------------------------------------------------------
        println!("== KeepOriginal");
        let meter = Arc::new(Meter::default());
        let t = Instant::now();
        let s = ctx.platform.start(ctx.single(HideMethod::KeepOriginal, &meter))?;
        println!("  start() returned in {:.1} ms", t.elapsed().as_secs_f32() * 1000.0);
        wait_sources(s.as_ref(), &[1], |x| x.state == SessionState::Running, 5.0)?;
        std::thread::sleep(Duration::from_millis(300));
        let rms = meter.rms_over(1.5);
        let (v, _) = volume_of(ctx.tone.pid)?;
        println!("  captured rms {rms:.6} (expected {expect:.6}, {:+.2} dB), tone volume {v}", db(rms) - db(expect));
        print_status(s.as_ref());
        ensure!((db(rms) - db(expect)).abs() < 1.0, "KeepOriginal capture level is off");
        ensure!((v - orig_v).abs() < 1e-6, "KeepOriginal must not change the volume");
        ensure!(!src(&s.status(), 1)?.original_hidden);
        s.stop()?;

        // -- 3. Hide ----------------------------------------------------------------------
        println!("== Hide (Auto)");
        let meter = Arc::new(Meter::default());
        let t = Instant::now();
        let s = ctx.platform.start(ctx.single(HideMethod::Auto, &meter))?;
        println!("  start() returned in {:.1} ms", t.elapsed().as_secs_f32() * 1000.0);
        wait_hidden(s.as_ref(), &[1], 5.0)?;
        println!("  running and hidden {:.0} ms after start()", t.elapsed().as_secs_f32() * 1000.0);
        std::thread::sleep(Duration::from_millis(300));
        let rms = meter.rms_over(2.0);
        let (v, m) = volume_of(ctx.tone.pid)?;
        println!(
            "  captured rms {rms:.6} (expected {expect:.6}, {:+.3} dB); tone session volume {v:e} muted={m}",
            db(rms) - db(expect)
        );
        print_status(s.as_ref());
        ensure!((db(rms) - db(expect)).abs() < 1.0, "makeup gain does not restore the tone's level within 1 dB");
        ensure!((0.5e-6..=2e-6).contains(&v), "the tone's session volume should be ~1e-6 while hidden");
        ensure!(!m, "hushed, not muted");
        let st = s.status();
        let x = src(&st, 1)?;
        ensure!(x.original_hidden && st.sample_rate > 0 && x.latency_ms > st.latency_ms && st.latency_ms > 0.0);
        let t = Instant::now();
        s.stop()?;
        let (v, m) = volume_of(ctx.tone.pid)?;
        println!("  stop() took {:.1} ms; tone volume after stop {v} muted={m}", t.elapsed().as_secs_f32() * 1000.0);
        ensure!((v - orig_v).abs() < 1e-4, "stop() must restore the original volume");

        println!("== dropping a hiding session without stop()");
        let meter = Arc::new(Meter::default());
        let s = ctx.platform.start(ctx.single(HideMethod::Auto, &meter))?;
        wait_hidden(s.as_ref(), &[1], 5.0)?;
        ensure!(volume_of(ctx.tone.pid)?.0 < 1e-5, "hushed");
        drop(s);
        let (v, _) = volume_of(ctx.tone.pid)?;
        println!("  tone volume after drop {v}");
        ensure!((v - orig_v).abs() < 1e-4, "Drop must restore the original volume");

        // -- 4. The app opens audio after Sidestage attached --------------------------------
        println!("== the app opens its audio only after Sidestage attached");
        ctx.tone.kill();
        ctx.tone = Tone::start_with(&["--delay-ms", "2500"])?;
        let meter = Arc::new(Meter::default());
        let s = ctx.platform.start(ctx.single(HideMethod::Auto, &meter))?;
        std::thread::sleep(Duration::from_millis(700));
        let st = s.status();
        let x = src(&st, 1)?;
        println!("  before the app has a stream: {:?}, hidden={}", x.state, x.original_hidden);
        ensure!(
            x.state == SessionState::WaitingForApp && sessions_of(ctx.tone.pid)?.is_empty(),
            "a running app without an audio stream is 'waiting'"
        );
        wait_for_session(ctx.tone.pid)?;
        let t = Instant::now();
        wait_hidden(s.as_ref(), &[1], 5.0)?;
        let hushed_after = t.elapsed();
        std::thread::sleep(Duration::from_millis(300));
        let rms = meter.rms_over(1.5);
        let (v, _) = volume_of(ctx.tone.pid)?;
        println!(
            "  the new stream was hidden {:.0} ms after it appeared; volume {v:e}; captured rms {rms:.6} ({:+.3} dB)",
            hushed_after.as_secs_f32() * 1000.0,
            db(rms) - db(expect)
        );
        ensure!((0.5e-6..=2e-6).contains(&v), "the new stream is hushed");
        ensure!((db(rms) - db(expect)).abs() < 1.0, "level of the late stream");
        s.stop()?;
        let (v, _) = volume_of(ctx.tone.pid)?;
        println!("  after stop: volume {v}");
        ensure!((v - orig_v).abs() < 1e-4, "stop() restores the late stream");

        // -- 5. Foreign change, restart, re-attach ------------------------------------------
        println!("== foreign volume change and re-attach");
        let meter = Arc::new(Meter::default());
        let s = ctx.platform.start(ctx.single(HideMethod::Auto, &meter))?;
        wait_hidden(s.as_ref(), &[1], 5.0)?;
        std::thread::sleep(Duration::from_millis(300));
        println!("  setting the tone's volume to 0.5 from outside Sidestage");
        set_volume_of(ctx.tone.pid, 0.5, false)?;
        let t = Instant::now();
        let mut rehushed = None;
        while t.elapsed() < Duration::from_secs(3) {
            let (v, _) = volume_of(ctx.tone.pid)?;
            if v < 1e-5
                && src(&s.status(), 1)?.state == SessionState::Running
                && meter.blocks.load(Ordering::Relaxed) > 0
            {
                rehushed = Some(t.elapsed());
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let rehushed = rehushed.context("the foreign change was not re-hushed")?;
        std::thread::sleep(Duration::from_millis(500));
        let rms = meter.rms_over(1.5);
        let want = expect * 0.5;
        println!(
            "  re-hushed after {:.0} ms; captured rms {rms:.6} (expected {want:.6}, {:+.3} dB)",
            rehushed.as_secs_f32() * 1000.0,
            db(rms) - db(want)
        );
        ensure!((db(rms) - db(want)).abs() < 1.0, "the adopted volume is not applied");

        println!("  dragging the slider: 0.20 .. 0.50 in 30 ms steps");
        for i in 0..7 {
            set_volume_of(ctx.tone.pid, 0.2 + 0.05 * i as f32, false)?;
            std::thread::sleep(Duration::from_millis(30));
        }
        wait_hushed(ctx.tone.pid, 3.0)?;
        std::thread::sleep(Duration::from_millis(500));
        let rms = meter.rms_over(1.0);
        let warnings = src(&s.status(), 1)?.warnings.clone();
        println!(
            "  after the drag: captured rms {rms:.6} ({:+.3} dB vs 0.5x), warnings {warnings:?}",
            db(rms) - db(want)
        );
        ensure!((db(rms) - db(want)).abs() < 1.0, "level after the drag");
        ensure!(!warnings.iter().any(|w| w.contains("keeps changing")), "a drag is not a fight");

        println!("  an app that resets its volume right after every hush");
        // The first change comes long after the last hush; the next three are fights.
        for _ in 0..4 {
            wait_hushed(ctx.tone.pid, 5.0)?;
            set_volume_of(ctx.tone.pid, 0.5, false)?;
        }
        std::thread::sleep(Duration::from_millis(100));
        let warnings = src(&s.status(), 1)?.warnings.clone();
        println!("  warnings: {warnings:?}");
        ensure!(warnings.iter().any(|w| w.contains("keeps changing")), "fighting is reported");
        wait_hushed(ctx.tone.pid, 5.0)?;

        println!("  restarting the tone");
        ctx.tone.kill();
        wait_sources(s.as_ref(), &[1], |x| x.state == SessionState::WaitingForApp, 5.0)?;
        println!("  state WaitingForApp");
        ctx.tone = Tone::start()?;
        wait_for_session(ctx.tone.pid)?;
        let took = wait_hidden(s.as_ref(), &[1], 8.0)?;
        std::thread::sleep(Duration::from_millis(500));
        let rms = meter.rms_over(1.5);
        let (v, _) = volume_of(ctx.tone.pid)?;
        println!(
            "  re-attached after {:.0} ms; tone volume {v:e}; captured rms {rms:.6} (expected {want:.6}, {:+.3} dB); processors built {}",
            took * 1000.0,
            db(rms) - db(want),
            meter.built.load(Ordering::Relaxed)
        );
        print_status(s.as_ref());
        ensure!((0.5e-6..=2e-6).contains(&v), "the restarted tone must be hushed");
        ensure!((db(rms) - db(want)).abs() < 1.0, "level after re-attach");
        ensure!(meter.built.load(Ordering::Relaxed) == 1, "the processor survives an app restart (tails ring out)");
        ensure!(src(&s.status(), 1)?.dropouts == 0, "an app restart is not a dropout");
        s.stop()?;
        let (v, _) = volume_of(ctx.tone.pid)?;
        println!("  after stop: tone volume {v} (the adopted 0.5)");
        ensure!((v - 0.5).abs() < 1e-4, "stop() restores the adopted volume");
        set_volume_of(ctx.tone.pid, ctx.original.0, ctx.original.1)?;

        // -- 6. Another instance -----------------------------------------------------------
        other_instance(ctx)?;

        // -- 7. Crash -----------------------------------------------------------------------
        crash_checks(ctx)
    }

    /// A second instance of the app is not captured. It keeps its own volume, except when
    /// Windows starts it at the hushed volume (Windows gives a new session the volume of the
    /// app's last session that closed); then it gets the app's volume back.
    fn other_instance(ctx: &mut Ctx) -> anyhow::Result<()> {
        println!("== another instance of the app");
        let (orig_v, _) = ctx.original;
        let expect = ctx.tone.rms();
        let meter = Arc::new(Meter::default());
        let s = ctx.platform.start(ctx.single(HideMethod::Auto, &meter))?;
        let r = (|| -> anyhow::Result<()> {
            wait_hidden(s.as_ref(), &[1], 5.0)?;
            let second = Tone::start()?;
            wait_for_session(second.pid)?;
            std::thread::sleep(Duration::from_millis(1500));
            let (v1, v2) = (volume_of(ctx.tone.pid)?.0, volume_of(second.pid)?.0);
            println!("  first instance volume {v1:e} (captured), second {v2}");
            ensure!(v1 < 1e-5, "the captured instance stays hushed");
            ensure!((v2 - orig_v).abs() < 1e-4, "the second instance is left alone");
            let warnings = src(&s.status(), 1)?.warnings.clone();
            println!("  warnings: {warnings:?}");
            ensure!(warnings.iter().any(|w| w.contains("More than one")), "the uncaptured instance is pointed out");
            set_volume_of(second.pid, 1e-6, false)?;
            let back =
                wait_volume(second.pid, orig_v, 4.0).context("the second instance stayed at the hushed volume")?;
            println!(
                "  second instance at the hushed volume: back to {orig_v} after {:.0} ms",
                back.as_secs_f32() * 1000.0
            );
            set_volume_of(second.pid, 0.0, false)?;
            std::thread::sleep(Duration::from_millis(1500));
            let v2 = volume_of(second.pid)?.0;
            println!("  second instance set to 0 by the user: {v2}");
            ensure!(v2 == 0.0, "a deliberate 0 is left alone");
            set_volume_of(second.pid, orig_v, false)?;
            std::thread::sleep(Duration::from_millis(300));
            let rms = meter.rms_over(1.0);
            println!("  captured rms {rms:.6} ({:+.3} dB): only the first instance", db(rms) - db(expect));
            ensure!((db(rms) - db(expect)).abs() < 1.0, "only one instance is captured");
            ensure!(volume_of(ctx.tone.pid)?.0 < 1e-5, "still hushed");
            Ok(())
        })();
        let stopped = s.stop();
        let (v, _) = volume_of(ctx.tone.pid)?;
        println!("  after stop: first instance volume {v}");
        r?;
        stopped?;
        ensure!((v - orig_v).abs() < 1e-4, "stop() restores the first instance");
        Ok(())
    }

    fn crash_checks(ctx: &mut Ctx) -> anyhow::Result<()> {
        let (orig_v, _) = ctx.original;
        println!("== crash: end a hiding Sidestage process and its whole process tree");
        let mut child = spawn_child_session(ctx.output.as_deref(), std::slice::from_ref(&ctx.key))?;
        let (v, _) = volume_of(ctx.tone.pid)?;
        let tree = descendants(child.id());
        println!("  child {} is hiding the tone (volume {v:e}); its process tree: {tree:?}", child.id());
        ensure!(v < 1e-5, "child hushed the tone");
        ensure!(
            !tree.iter().any(|(_, exe)| exe.eq_ignore_ascii_case("win_e2e.exe")),
            "the guardian must not be in Sidestage's process tree"
        );
        kill_tree(child.id());
        let _ = child.wait();
        let restored = wait_volume(ctx.tone.pid, orig_v, 10.0).context("the guardian did not restore the tone")?;
        println!("  guardian restored the volume {:.0} ms after the kill", restored.as_secs_f32() * 1000.0);
        ensure!(restored < Duration::from_secs(5), "restore must happen within a few seconds");

        println!("== recover");
        std::thread::sleep(Duration::from_millis(300));
        let lines = ctx.platform.recover()?;
        println!("  recover(): {lines:?}");
        ensure!(lines.is_empty(), "nothing should be left to recover");
        let journal = sidestage_platform::journal::state_dir()?.join("windows-hush.json");
        ensure!(!journal.exists(), "the journal is cleared");

        println!("== crash while the app is closed; recover(); restore when it plays again");
        let mut child = spawn_child_session(ctx.output.as_deref(), std::slice::from_ref(&ctx.key))?;
        ctx.tone.kill();
        std::thread::sleep(Duration::from_millis(700));
        println!("  tone closed while hidden; killing the hiding process");
        kill_tree(child.id());
        let _ = child.wait();
        std::thread::sleep(Duration::from_millis(1000));
        let lines = ctx.platform.recover()?;
        println!("  recover(): {lines:?}");
        ensure!(lines.len() == 1 && journal.exists(), "the closed app stays journaled for later");
        // Give the dead process's guardian time to see that we own the journal now.
        std::thread::sleep(Duration::from_millis(1500));
        ctx.tone = Tone::start()?;
        wait_for_session(ctx.tone.pid)?;
        let (v0, _) = volume_of(ctx.tone.pid)?;
        let restored = wait_volume(ctx.tone.pid, orig_v, 8.0).context("the relaunched tone was not restored")?;
        println!(
            "  relaunched tone came up at volume {v0:e}; restored {:.0} ms later",
            restored.as_secs_f32() * 1000.0
        );
        std::thread::sleep(Duration::from_millis(300));
        let lines = ctx.platform.recover()?;
        ensure!(lines.is_empty() && !journal.exists(), "nothing left after the restore");

        // This process has nothing pending now, so it leaves the journal to the dead run's
        // guardian.
        println!("== crash while the app is closed; the guardian restores it when it plays");
        let mut child = spawn_child_session(ctx.output.as_deref(), std::slice::from_ref(&ctx.key))?;
        ctx.tone.kill();
        std::thread::sleep(Duration::from_millis(700));
        kill_tree(child.id());
        let _ = child.wait();
        std::thread::sleep(Duration::from_millis(1500));
        ensure!(journal.exists(), "the closed app stays journaled");
        let started = Instant::now();
        ctx.tone = Tone::start()?;
        let mut restored = None;
        while started.elapsed() < Duration::from_secs(5) {
            if !sessions_of(ctx.tone.pid)?.is_empty() && (volume_of(ctx.tone.pid)?.0 - orig_v).abs() < 1e-4 {
                restored = Some(started.elapsed());
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let restored = restored.context("the guardian did not restore the relaunched tone")?;
        println!(
            "  the guardian restored the relaunched tone {:.0} ms after it was started",
            restored.as_secs_f32() * 1000.0
        );
        // Polling alone takes up to a second here (10 s after the first two minutes).
        ensure!(restored < Duration::from_millis(500), "the guardian should be woken by the new audio session");
        let t = Instant::now();
        while journal.exists() && t.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(20));
        }
        ensure!(!journal.exists(), "the guardian clears the journal");
        Ok(())
    }

    // ---- several sources ---------------------------------------------------------------------

    /// The tones of the multi-source checks: (exe name, frequency, amplitude, processor gain).
    const VOICES: [(&str, f32, f32, f32); 4] = [
        ("win_tone_a", 500.0, 0.01, 0.4),
        ("win_tone_b", 997.0, 0.006, 0.3),
        ("win_tone_c", 1300.0, 0.004, 0.2),
        ("win_tone_d", 1700.0, 0.008, 0.1),
    ];

    struct Voice {
        tone: Tone,
        original: (f32, bool),
        meter: Arc<Meter>,
        gain: f32,
    }

    impl Voice {
        fn start(i: usize) -> anyhow::Result<Voice> {
            let (name, freq, amp, gain) = VOICES[i];
            let tone = Tone::named(name, freq, amp, &[])?;
            let original = usable_volume(tone.pid)?;
            Ok(Voice { tone, original, meter: Arc::new(Meter::default()), gain })
        }

        fn config(&self, id: SourceId) -> SourceConfig {
            source(id, self.tone.key(), HideMethod::Auto, probe_factory(&self.meter, self.gain, 0.0))
        }

        fn restore(&self) {
            let _ = set_volume_of(self.tone.pid, self.original.0, self.original.1);
        }
    }

    /// Puts the voices' volumes back even if a check failed.
    struct Voices(Vec<Voice>);

    impl Drop for Voices {
        fn drop(&mut self) {
            for v in &self.0 {
                v.restore();
            }
        }
    }

    fn multi_source_checks(ctx: &mut Ctx) -> anyhow::Result<()> {
        let mut voices = Voices(vec![Voice::start(0)?, Voice::start(1)?, Voice::start(2)?, Voice::start(3)?]);

        // -- 8. Problems of one source, invalid lists -------------------------------------------
        println!("== Sidestage itself, and an app whose processor cannot be built, as sources");
        let me = AppKey(std::env::current_exe()?.to_string_lossy().to_lowercase());
        let meter = Arc::new(Meter::default());
        let mut cfg = ctx.single(HideMethod::Auto, &meter);
        cfg.sources.push(source(2, me.clone(), HideMethod::Auto, factory(&meter)));
        let broken: ProcessorFactory = Arc::new(|_| panic!("test: this processor cannot be built"));
        cfg.sources.push(source(3, voices.0[3].tone.key(), HideMethod::Auto, broken));
        // Breaks the processor contract: panics on the real-time thread after 0.5 s.
        let panicking: ProcessorFactory = Arc::new(|_| {
            struct Panics(u32);
            impl Processor for Panics {
                fn process(&mut self, _input: &[f32], output: &mut [f32], _sidechain_peak: f32) {
                    self.0 += 1;
                    assert!(self.0 < 50, "test: a processor that panics while playing");
                    output.fill(0.0);
                }
            }
            Box::new(Panics(0)) as Box<dyn Processor>
        });
        cfg.sources.push(source(4, voices.0[2].tone.key(), HideMethod::Auto, panicking));
        let s = ctx.platform.start(cfg)?;
        wait_hidden(s.as_ref(), &[1], 5.0)?;
        std::thread::sleep(Duration::from_millis(500));
        let st = s.status();
        let own = src(&st, 2)?;
        let bad = src(&st, 3)?;
        println!("  source 2 (Sidestage itself): {:?}", own.state);
        println!(
            "  source 3 (no processor): {:?} hidden={} warnings {:?}",
            bad.state, bad.original_hidden, bad.warnings
        );
        println!("  source 1: {:?} hidden={}", src(&st, 1)?.state, src(&st, 1)?.original_hidden);
        ensure!(
            matches!(&own.state, SessionState::Error(e) if e.contains("cannot capture itself")),
            "capturing Sidestage itself is that source's error"
        );
        ensure!(
            !bad.original_hidden && bad.warnings.iter().any(|w| w.contains("processor could not be created")),
            "a processor that cannot be built is that source's problem"
        );
        ensure!(
            (volume_of(voices.0[3].tone.pid)?.0 - voices.0[3].original.0).abs() < 1e-6,
            "an app that cannot be played stays audible"
        );
        let died = wait_sources(
            s.as_ref(),
            &[4],
            |x| !x.original_hidden && x.warnings.iter().any(|w| w.contains("stopped working")),
            5.0,
        )?;
        let back = wait_volume(voices.0[2].tone.pid, voices.0[2].original.0, 2.0)?;
        let st = s.status();
        println!(
            "  source 4 (processor panics while playing): {:?} after {:.0} ms, original audible again; warnings {:?}",
            src(&st, 4)?.state,
            died * 1000.0,
            src(&st, 4)?.warnings
        );
        println!(
            "  its volume was back {:.0} ms later; source 1: {:?}",
            back.as_secs_f32() * 1000.0,
            src(&st, 1)?.state
        );
        ensure!(running_hidden(src(&st, 1)?), "the other sources keep playing");
        ensure!(st.state == SessionState::Running, "the output keeps playing");
        s.stop()?;
        let park = SessionConfig {
            sources: vec![source(1, ctx.key.clone(), HideMethod::ParkOn("x".into()), factory(&meter))],
            ..ctx.single(HideMethod::Auto, &meter)
        };
        match ctx.platform.start(park) {
            Ok(_) => bail!("ParkOn must be refused"),
            Err(e) => println!("  ParkOn is refused: {e:#}"),
        }

        let mix = MixMeter::new(&VOICES.map(|v| v.1));

        // -- 9. Three sources ------------------------------------------------------------------
        println!("== three sources at different levels");
        let cfg = SessionConfig {
            output: ctx.output.clone(),
            sidechain: true,
            sidechain_ignore: ctx.others.clone(),
            sources: vec![voices.0[0].config(1), voices.0[1].config(2), voices.0[2].config(3)],
            master: mix.factory(0.0),
        };
        let t = Instant::now();
        let mut s = ctx.platform.start(cfg)?;
        println!("  start() returned in {:.1} ms", t.elapsed().as_secs_f32() * 1000.0);
        let r = (|| -> anyhow::Result<()> {
            let v = &mut voices.0;
            wait_hidden(s.as_ref(), &[1, 2, 3], 5.0)?;
            println!("  all three running and hidden {:.0} ms after start()", t.elapsed().as_secs_f32() * 1000.0);
            std::thread::sleep(Duration::from_millis(300));
            check_levels(&[&v[0], &v[1], &v[2]], &[true, true, true, false], &mix)?;
            print_status(s.as_ref());

            // -- 10. Add one, remove one, live ---------------------------------------------
            println!("== set_sources: add a fourth source and remove the second while the others play");
            let (gaps0, built0, drops0) = untouched(s.as_ref(), &v[..], &[(1, 0), (3, 2)])?;
            let watch = VolumeWatch::start(vec![v[0].tone.pid, v[2].tone.pid]);
            std::thread::sleep(Duration::from_millis(300));
            let t = Instant::now();
            s.set_sources(vec![v[0].config(1), v[2].config(3), v[3].config(4)])?;
            let returned = t.elapsed();
            let removed = wait_volume(v[1].tone.pid, v[1].original.0, 3.0).context("the removed source is restored")?;
            let added = wait_hidden(s.as_ref(), &[4], 5.0)?;
            std::thread::sleep(Duration::from_millis(700));
            let watched = watch.finish()?;
            let (gaps1, built1, drops1) = untouched(s.as_ref(), &v[..], &[(1, 0), (3, 2)])?;
            println!(
                "  set_sources returned in {:.1} ms; the removed source's volume was back after {:.0} ms; the new one was hidden after {:.0} ms",
                returned.as_secs_f32() * 1000.0,
                removed.as_secs_f32() * 1000.0,
                added * 1000.0
            );
            for (k, (lo, hi, n)) in watched.iter().enumerate() {
                println!("  untouched source {}: session volume {lo:e} .. {hi:e} over {n} readings", [1, 3][k]);
                ensure!(*n > 100 && *lo > 0.0 && *hi <= 2e-6, "an untouched source must stay hushed (and unmuted)");
            }
            println!(
                "  untouched sources: silent blocks {gaps0:?} -> {gaps1:?}, processors built {built0:?} -> {built1:?}, dropouts {drops0:?} -> {drops1:?}"
            );
            ensure!(gaps0 == gaps1, "the untouched sources' audio has no gap");
            ensure!(built0 == built1, "the untouched sources' processors are not rebuilt");
            ensure!(drops0 == drops1, "the untouched sources have no dropouts");
            ensure!(v[3].meter.built.load(Ordering::Relaxed) == 1, "the new source got one processor");
            let st = s.status();
            ensure!(st.sources.iter().map(|x| x.id).collect::<Vec<_>>() == [1, 3, 4], "status lists the new sources");
            check_levels(&[&v[0], &v[2], &v[3]], &[true, false, true, true], &mix)?;

            println!("== invalid source lists are refused while the session runs");
            let dup_app =
                vec![v[0].config(1), v[2].config(3), source(9, v[0].tone.key(), HideMethod::Auto, factory(&meter))];
            let e = s.set_sources(dup_app).err().context("a duplicate app must be refused")?;
            println!("  duplicate app: {e:#}");
            let dup_id = vec![v[0].config(1), v[2].config(1)];
            let e = s.set_sources(dup_id).err().context("a duplicate ID must be refused")?;
            println!("  duplicate ID: {e:#}");
            std::thread::sleep(Duration::from_millis(200));
            let st = s.status();
            ensure!(
                st.sources.iter().map(|x| x.id).collect::<Vec<_>>() == [1, 3, 4]
                    && st.sources.iter().all(running_hidden),
                "a refused list changes nothing"
            );

            // -- 11. Soak -------------------------------------------------------------------
            soak(s.as_ref(), &[1, 3, 4], Duration::from_secs(40))?;

            println!("== a source removed while its app is closed is restored when the app plays again");
            v[2].tone.kill();
            wait_sources(s.as_ref(), &[3], |x| x.state == SessionState::WaitingForApp, 5.0)?;
            ensure!(wait_hidden(s.as_ref(), &[1, 4], 0.5).is_ok(), "the other sources keep playing");
            s.set_sources(vec![v[0].config(1), v[3].config(4)])?;
            std::thread::sleep(Duration::from_millis(300));
            let journal = sidestage_platform::journal::state_dir()?.join("windows-hush.json");
            let has_c = || std::fs::read_to_string(&journal).is_ok_and(|j| j.contains("win_tone_c"));
            ensure!(has_c(), "the closed app's entry stays journaled");
            v[2].tone = Tone::named(VOICES[2].0, VOICES[2].1, VOICES[2].2, &[])?;
            wait_for_session(v[2].tone.pid)?;
            let came = volume_of(v[2].tone.pid)?.0;
            let back = wait_volume(v[2].tone.pid, v[2].original.0, 4.0)?;
            std::thread::sleep(Duration::from_millis(300));
            println!(
                "  the app came back at volume {came:e}; restored after {:.0} ms; journal entry gone: {}",
                back.as_secs_f32() * 1000.0,
                !has_c()
            );
            ensure!(!has_c(), "the restored entry leaves the journal");
            ensure!(wait_hidden(s.as_ref(), &[1, 4], 0.5).is_ok(), "the other sources are not touched");
            Ok(())
        })();
        let v = &voices.0;
        let t = Instant::now();
        let stopped = s.stop();
        println!("  stop() took {:.1} ms", t.elapsed().as_secs_f32() * 1000.0);
        r?;
        stopped?;
        for x in v {
            let (vol, m) = volume_of(x.tone.pid)?;
            ensure!((vol - x.original.0).abs() < 1e-4 && m == x.original.1, "stop() restores every source");
        }
        println!("  every source's volume is back");

        // -- 12. Crash with three hidden sources ----------------------------------------------
        println!("== crash with three hidden sources");
        let keys = [v[0].tone.key(), v[1].tone.key(), v[2].tone.key()];
        let mut child = spawn_child_session(ctx.output.as_deref(), &keys)?;
        for x in &v[..3] {
            ensure!(volume_of(x.tone.pid)?.0 < 1e-5, "the child hushed every source");
        }
        let journal = sidestage_platform::journal::state_dir()?.join("windows-hush.json");
        let entries = std::fs::read_to_string(&journal).map(|j| j.matches("\"session_id\"").count()).unwrap_or(0);
        println!("  child {} hides three tones; journal entries: {entries}", child.id());
        ensure!(entries >= 3, "one journal holds every hidden app");
        kill_tree(child.id());
        let _ = child.wait();
        let t = Instant::now();
        for x in &v[..3] {
            wait_volume(x.tone.pid, x.original.0, 10.0).context("the guardian restores every source")?;
        }
        println!("  the guardian restored all three {:.0} ms after the kill", t.elapsed().as_secs_f32() * 1000.0);
        ensure!(t.elapsed() < Duration::from_secs(5), "restore must happen within a few seconds");
        let t = Instant::now();
        while journal.exists() && t.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(20));
        }
        ensure!(!journal.exists(), "the guardian clears the journal");
        ensure!(ctx.platform.recover()?.is_empty(), "nothing left to recover");
        crash_while_churning(ctx.platform.as_mut(), ctx.output.as_deref(), &v[..3], 12)?;

        // -- 14. The output dies and is reopened --------------------------------------------
        output_restart_checks(ctx.platform.as_mut(), ctx.output.clone(), ctx.others.clone(), &v[..3])?;
        drop(voices);

        // -- 13. Sidechain --------------------------------------------------------------------
        sidechain_checks(ctx.platform.as_mut(), ctx.output.clone(), ctx.others.clone())
    }

    /// Silent blocks, processors built and dropouts of the sources `(id, voice index)`.
    #[allow(clippy::type_complexity)]
    fn untouched(
        s: &dyn Session,
        v: &[Voice],
        which: &[(SourceId, usize)],
    ) -> anyhow::Result<(Vec<u64>, Vec<u64>, Vec<u64>)> {
        let st = s.status();
        let mut out = (Vec::new(), Vec::new(), Vec::new());
        for &(id, i) in which {
            out.0.push(v[i].meter.gaps.load(Ordering::Relaxed));
            out.1.push(v[i].meter.built.load(Ordering::Relaxed));
            out.2.push(src(&st, id)?.dropouts);
        }
        Ok(out)
    }

    /// Each playing voice's capture is at its tone's level, and the master's input holds each
    /// of them at tone level times the source's gain (`present` by index of `VOICES`).
    fn check_levels(playing: &[&Voice], present: &[bool; 4], mix: &MixMeter) -> anyhow::Result<()> {
        let measured: Vec<f32> = {
            let snaps: Vec<(f64, u64)> = playing.iter().map(|v| v.meter.snapshot()).collect();
            let amps = mix.amplitudes(2.0);
            for (v, (s0, n0)) in playing.iter().zip(&snaps) {
                let (s1, n1) = v.meter.snapshot();
                let rms = if n1 <= *n0 { 0.0 } else { ((s1 - s0) / (n1 - n0) as f64).max(0.0).sqrt() as f32 };
                println!(
                    "  {} ({} Hz): captured rms {rms:.6} (expected {:.6}, {:+.3} dB)",
                    exe_stem(&v.tone.path),
                    v.tone.freq,
                    v.tone.rms(),
                    db(rms) - db(v.tone.rms())
                );
                ensure!((db(rms) - db(v.tone.rms())).abs() < 1.0, "a source is not captured at its own level");
            }
            amps
        };
        for (k, (name, freq, _, gain)) in VOICES.iter().enumerate() {
            let tone_amp = playing.iter().find(|v| exe_stem(&v.tone.path) == *name).map(|v| v.tone.amp);
            let want = if present[k] { tone_amp.unwrap_or(0.0) * gain } else { 0.0 };
            println!("  mix at {freq} Hz: {:.6} (expected {want:.6})", measured[k]);
            if want > 0.0 {
                ensure!((db(measured[k]) - db(want)).abs() < 1.0, "the mix holds a source at the wrong level");
            } else {
                ensure!(measured[k] < 1e-5, "a source that is not in the session is in the mix");
            }
        }
        Ok(())
    }

    fn exe_stem(path: &str) -> String {
        std::path::Path::new(path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    }

    /// Plays for `len` and reports latency, drift and dropouts; there must be none.
    fn soak(s: &dyn Session, ids: &[SourceId], len: Duration) -> anyhow::Result<()> {
        println!("== soak: {} sources for {} s", ids.len(), len.as_secs());
        let cpu0 = cpu_time();
        let st0 = s.status();
        let t = Instant::now();
        let mut worst_fill = 0f32;
        while t.elapsed() < len {
            std::thread::sleep(Duration::from_secs(10).min(len.saturating_sub(t.elapsed())));
            let st = s.status();
            let line: Vec<String> = st
                .sources
                .iter()
                .map(|x| {
                    worst_fill = worst_fill.max(x.latency_ms);
                    format!("#{} {:.1} ms {:+.1} ppm {} drops", x.id, x.latency_ms, x.drift_ppm, x.dropouts)
                })
                .collect();
            println!("  t={:4.0}s output dropouts {} | {}", t.elapsed().as_secs_f32(), st.dropouts, line.join(" | "));
        }
        let cpu = cpu_time().saturating_sub(cpu0).as_secs_f32() / t.elapsed().as_secs_f32() * 100.0;
        let st = s.status();
        println!("  CPU of this process: {cpu:.2}% of one core; worst source latency {worst_fill:.1} ms");
        ensure!(st.dropouts == st0.dropouts, "output dropouts during the soak");
        for &id in ids {
            ensure!(src(&st, id)?.dropouts == src(&st0, id)?.dropouts, "source {id} had dropouts during the soak");
            ensure!(running_hidden(src(&st, id)?), "source {id} stopped during the soak");
        }
        Ok(())
    }

    /// CPU use of this process over the next `secs`, in percent of one core.
    fn unix_ms() -> u128 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis())
    }

    fn cpu_over(secs: f32) -> f32 {
        let (c0, t0) = (cpu_time(), Instant::now());
        std::thread::sleep(Duration::from_secs_f32(secs));
        cpu_time().saturating_sub(c0).as_secs_f32() / t0.elapsed().as_secs_f32() * 100.0
    }

    /// User plus kernel CPU time of this process.
    fn cpu_time() -> Duration {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
        let (mut c, mut e, mut k, mut u) =
            (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        if unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) }.is_err() {
            return Duration::ZERO;
        }
        let t = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
        Duration::from_nanos((t(k) + t(u)) * 100)
    }

    // ---- crash helpers -----------------------------------------------------------------------

    fn created(pid: u32) -> Option<u64> {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        let h = OwnedHandle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?);
        let (mut c, mut e, mut k, mut u) =
            (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        unsafe { GetProcessTimes(h.raw(), &mut c, &mut e, &mut k, &mut u) }.ok()?;
        Some((u64::from(c.dwHighDateTime) << 32) | u64::from(c.dwLowDateTime))
    }

    /// Every live descendant of `pid` (PID and executable name), as "End process tree" sees
    /// them.
    fn descendants(pid: u32) -> Vec<(u32, String)> {
        use windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
        };
        let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
            return Vec::new();
        };
        let snap = OwnedHandle(snap);
        let mut all = Vec::new();
        let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = unsafe { Process32FirstW(snap.raw(), &mut e) }.is_ok();
        while ok {
            let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
            all.push((e.th32ProcessID, e.th32ParentProcessID, String::from_utf16_lossy(&e.szExeFile[..n])));
            ok = unsafe { Process32NextW(snap.raw(), &mut e) }.is_ok();
        }
        let mut out: Vec<(u32, String)> = Vec::new();
        let mut stack = vec![pid];
        while let Some(p) = stack.pop() {
            let born = created(p);
            for (child, parent, exe) in &all {
                // A child is younger than its parent (its parent PID may be stale otherwise).
                if *parent == p
                    && *child != p
                    && !out.iter().any(|(c, _)| c == child)
                    && born.is_some_and(|b| created(*child).is_some_and(|c| c >= b))
                {
                    out.push((*child, exe.clone()));
                    stack.push(*child);
                }
            }
        }
        out
    }

    /// Ends `pid` and every descendant, like Task Manager's "End process tree".
    fn kill_tree(pid: u32) {
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE};
        let mut victims = descendants(pid);
        victims.insert(0, (pid, String::new()));
        for (p, _) in victims {
            if let Ok(h) = unsafe { OpenProcess(PROCESS_TERMINATE, false, p) } {
                let h = OwnedHandle(h);
                let _ = unsafe { TerminateProcess(h.raw(), 1) };
            }
        }
    }

    /// Starts `--child-session` hiding `keys` and waits until it reports that all are hidden.
    fn spawn_child_session(output: Option<&str>, keys: &[AppKey]) -> anyhow::Result<Child> {
        spawn_child("--child-session", output, keys)
    }

    /// Starts a child mode (`--child-session`, `--child-churn`) and waits for its HIDDEN line.
    fn spawn_child(mode: &str, output: Option<&str>, keys: &[AppKey]) -> anyhow::Result<Child> {
        let exe = std::env::current_exe()?;
        let mut args = vec![mode.to_owned(), output.unwrap_or("-").to_owned()];
        args.extend(keys.iter().map(|k| k.0.clone()));
        let mut child = Command::new(exe).args(&args).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn()?;
        let out = child.stdout.take().context("child stdout")?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
        let ready = loop {
            match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(l) if l.starts_with("HIDDEN") => break true,
                Ok(l) if l.starts_with("ERROR") => {
                    println!("  child: {l}");
                    break false;
                }
                Ok(_) => {}
                Err(_) => break false,
            }
        };
        if !ready {
            let _ = child.kill();
            bail!("the child session did not start");
        }
        Ok(child)
    }

    /// Kills a child session `rounds` times, each at a different moment while it keeps adding
    /// and removing sources (journal writes, hushes and restores in flight). Every time, the
    /// guardian must put every tone back and clear the journal.
    fn crash_while_churning(
        platform: &mut dyn Platform,
        output: Option<&str>,
        v: &[Voice],
        rounds: u64,
    ) -> anyhow::Result<()> {
        println!("== {rounds} crashes while sources are being added and removed");
        let keys: Vec<AppKey> = v.iter().map(|x| x.tone.key()).collect();
        let journal = sidestage_platform::journal::state_dir()?.join("windows-hush.json");
        let (mut worst, mut hushed_at_kill) = (Duration::ZERO, 0usize);
        for round in 0..rounds {
            let mut child = spawn_child("--child-churn", output, &keys)?;
            // Spread over the churn's 40-130 ms steps (and a hush's ~60 ms after each attach).
            std::thread::sleep(Duration::from_millis(30 + round * 397 % 900));
            kill_tree(child.id());
            let _ = child.wait();
            let t = Instant::now();
            let hushed = v.iter().filter(|x| volume_of(x.tone.pid).is_ok_and(|(vol, _)| vol < 1e-3)).count();
            hushed_at_kill += hushed;
            for x in v {
                wait_volume(x.tone.pid, x.original.0, 10.0)
                    .with_context(|| format!("round {round}: the guardian restores {}", exe_stem(&x.tone.path)))?;
            }
            let took = t.elapsed();
            worst = worst.max(took);
            while journal.exists() && t.elapsed() < Duration::from_secs(3) {
                std::thread::sleep(Duration::from_millis(20));
            }
            ensure!(!journal.exists(), "round {round}: the guardian clears the journal");
            for x in v {
                ensure!(volume_of(x.tone.pid)?.1 == x.original.1, "round {round}: mute state restored");
            }
            println!(
                "  round {round}: {hushed} tone(s) hidden at the kill; all back after {:.0} ms",
                took.as_secs_f32() * 1000.0
            );
        }
        println!(
            "  {rounds} kills ({hushed_at_kill} hidden tones in all): every tone back within {:.0} ms of each kill",
            worst.as_secs_f32() * 1000.0
        );
        ensure!(hushed_at_kill > 0, "the kills should catch hidden tones");
        ensure!(platform.recover()?.is_empty(), "nothing left to recover");
        Ok(())
    }

    pub fn churn_only(rounds: u64) -> i32 {
        if unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_err() {
            return 1;
        }
        let r = (|| -> anyhow::Result<()> {
            let voices = Voices(vec![Voice::start(0)?, Voice::start(1)?, Voice::start(2)?]);
            let mut platform = sidestage_platform::platform()?;
            let output = test_output(platform.as_mut())?;
            crash_while_churning(platform.as_mut(), output.as_deref(), &voices.0, rounds)
        })();
        match r {
            Ok(()) => {
                println!("PASS");
                0
            }
            Err(e) => {
                println!("FAIL: {e:#}");
                1
            }
        }
    }

    /// A master that works for a while, then panics on the render thread: the output thread ends
    /// exactly as it does when its device goes away.
    struct DiesAfter {
        inner: Box<dyn Processor>,
        frames_left: u64,
        died: Arc<Mutex<Option<Instant>>>,
    }

    impl Processor for DiesAfter {
        fn process(&mut self, input: &[f32], output: &mut [f32], sidechain_peak: f32) {
            let n = (input.len() / 2) as u64;
            if self.frames_left <= n {
                // Test only: breaks the processor contract on purpose.
                *self.died.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
                panic!("test: the output stream dies");
            }
            self.frames_left -= n;
            self.inner.process(input, output, sidechain_peak);
        }
    }

    /// The output stream dies while three sources play and has run long enough to count as
    /// healthy, which is what a device change or an unplugged headset does: the output is
    /// reopened and every source gets a new bridge and processor. Their apps must stay hidden
    /// throughout (no unhide blip), and the mix must be right again afterwards.
    fn output_restart_checks(
        platform: &mut dyn Platform,
        output: Option<String>,
        others: Vec<AppKey>,
        v: &[Voice],
    ) -> anyhow::Result<()> {
        println!("== the output dies after 11 s while three sources play, and is reopened");
        let mix = MixMeter::new(&VOICES.map(|v| v.1));
        let inner = mix.factory(0.0);
        let builds = Arc::new(AtomicU64::new(0));
        let died = Arc::new(Mutex::new(None::<Instant>));
        let master: ProcessorFactory = {
            let (builds, died) = (builds.clone(), died.clone());
            Arc::new(move |rate| {
                let p = inner(rate);
                if builds.fetch_add(1, Ordering::Relaxed) == 0 {
                    let frames_left = u64::from(rate) * 11;
                    Box::new(DiesAfter { inner: p, frames_left, died: died.clone() }) as Box<dyn Processor>
                } else {
                    p
                }
            })
        };
        let built0: Vec<u64> = v.iter().map(|x| x.meter.built.load(Ordering::Relaxed)).collect();
        let s = platform.start(SessionConfig {
            output,
            sidechain: true,
            sidechain_ignore: others,
            sources: v.iter().enumerate().map(|(i, x)| x.config(i as SourceId + 1)).collect(),
            master,
        })?;
        let r = (|| -> anyhow::Result<()> {
            let ids = [1, 2, 3];
            wait_hidden(s.as_ref(), &ids, 5.0)?;
            let watch = VolumeWatch::start(v.iter().map(|x| x.tone.pid).collect());
            let t = Instant::now();
            while builds.load(Ordering::Relaxed) < 2 && t.elapsed() < Duration::from_secs(20) {
                std::thread::sleep(Duration::from_millis(5));
            }
            ensure!(builds.load(Ordering::Relaxed) >= 2, "the output was not reopened");
            let died_at = died.lock().unwrap_or_else(|p| p.into_inner()).context("the master did not die")?;
            let reopened = died_at.elapsed();
            wait_hidden(s.as_ref(), &ids, 5.0)?;
            // Audible again: the new processors get the tone at its level (after the gate's
            // settle time).
            let mut audible = Vec::new();
            for x in v {
                while died_at.elapsed() < Duration::from_secs(5) && x.meter.rms_over(0.01) < x.tone.rms() * 0.5 {}
                audible.push(died_at.elapsed().as_secs_f32() * 1000.0);
            }
            ensure!(died_at.elapsed() < Duration::from_secs(5), "a source did not come back after the reopen");
            std::thread::sleep(Duration::from_millis(300));
            let watched = watch.finish()?;
            println!(
                "  the output was reopened {:.0} ms after it died; the sources were audible again after {:?} ms",
                reopened.as_secs_f32() * 1000.0,
                audible.iter().map(|a| a.round()).collect::<Vec<_>>()
            );
            for (k, (lo, hi, n)) in watched.iter().enumerate() {
                println!("  source {}: session volume {lo:e} .. {hi:e} over {n} readings", k + 1);
                ensure!(
                    *n > 100 && *lo > 0.0 && *hi <= 2e-6,
                    "a source's app must stay hidden while the output reopens"
                );
            }
            let built: Vec<u64> = v.iter().map(|x| x.meter.built.load(Ordering::Relaxed)).collect();
            println!("  processors built {built0:?} -> {built:?}");
            ensure!(
                built.iter().zip(&built0).all(|(b, b0)| *b == b0 + 2),
                "each source gets one processor per output stream"
            );
            let st = s.status();
            ensure!(
                st.state == SessionState::Running && st.warnings.iter().all(|w| !w.contains("output")),
                "the session recovered: {:?}",
                st.warnings
            );
            check_levels(&[&v[0], &v[1], &v[2]], &[true, true, true, false], &mix)?;
            print_status(s.as_ref());
            Ok(())
        })();
        let stopped = s.stop();
        r?;
        stopped?;
        for x in v {
            let (vol, m) = volume_of(x.tone.pid)?;
            ensure!((vol - x.original.0).abs() < 1e-4 && m == x.original.1, "stop() restores every source");
        }
        Ok(())
    }

    pub fn reopen_only() -> i32 {
        if unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_err() {
            return 1;
        }
        let r = (|| -> anyhow::Result<()> {
            let voices = Voices(vec![Voice::start(0)?, Voice::start(1)?, Voice::start(2)?]);
            let mut platform = sidestage_platform::platform()?;
            let output = test_output(platform.as_mut())?;
            let others = other_apps(platform.as_mut())?;
            output_restart_checks(platform.as_mut(), output, others, &voices.0)
        })();
        match r {
            Ok(()) => {
                println!("PASS");
                0
            }
            Err(e) => {
                println!("FAIL: {e:#}");
                1
            }
        }
    }

    /// Child mode for the churn crash test: starts with the first of three apps, reports, then
    /// keeps changing which of them are sources until it is killed.
    pub fn child_churn(args: &[String]) -> i32 {
        let r = (|| -> anyhow::Result<()> {
            let output = args.first().filter(|o| o.as_str() != "-").cloned();
            let keys = &args[1.min(args.len())..];
            ensure!(keys.len() == 3, "usage: --child-churn <output|-> <app key> <app key> <app key>");
            let mut platform = sidestage_platform::platform()?;
            let meter = Arc::new(Meter::default());
            let cfg = |which: &[usize]| -> Vec<SourceConfig> {
                which
                    .iter()
                    .map(|&i| source(i as SourceId + 1, AppKey(keys[i].clone()), HideMethod::Auto, factory(&meter)))
                    .collect()
            };
            let mut s = platform.start(SessionConfig {
                output,
                sidechain: false,
                sidechain_ignore: Vec::new(),
                sources: cfg(&[0]),
                master: silent_master(),
            })?;
            wait_hidden(s.as_ref(), &[1], 8.0)?;
            println!("HIDDEN");
            std::io::stdout().flush()?;
            const STEPS: [&[usize]; 6] = [&[0, 1], &[0, 1, 2], &[1, 2], &[2], &[0, 2], &[0]];
            for i in 0..2000u64 {
                s.set_sources(cfg(STEPS[i as usize % STEPS.len()]))?;
                std::thread::sleep(Duration::from_millis(40 + i * 37 % 90));
            }
            // Not reached in the test (the parent kills us); restore cleanly if it is.
            s.stop()
        })();
        match r {
            Ok(()) => 0,
            Err(e) => {
                println!("ERROR {e:#}");
                1
            }
        }
    }

    /// Child mode for the crash tests: hide the given apps, report, then wait to be killed.
    pub fn child_session(args: &[String]) -> i32 {
        let r = (|| -> anyhow::Result<()> {
            let output = args.first().filter(|o| o.as_str() != "-").cloned();
            let keys = &args[1.min(args.len())..];
            ensure!(!keys.is_empty(), "usage: --child-session <output|-> <app key>...");
            let mut platform = sidestage_platform::platform()?;
            let meter = Arc::new(Meter::default());
            let sources: Vec<SourceConfig> = keys
                .iter()
                .enumerate()
                .map(|(i, k)| source(i as SourceId + 1, AppKey(k.clone()), HideMethod::Auto, factory(&meter)))
                .collect();
            let ids: Vec<SourceId> = sources.iter().map(|s| s.id).collect();
            let s = platform.start(SessionConfig {
                output,
                sidechain: false,
                sidechain_ignore: Vec::new(),
                sources,
                master: silent_master(),
            })?;
            wait_hidden(s.as_ref(), &ids, 8.0)?;
            println!("HIDDEN");
            std::io::stdout().flush()?;
            std::thread::sleep(Duration::from_secs(60));
            // Not reached in the test (the parent kills us); restore cleanly if it is.
            s.stop()
        })();
        match r {
            Ok(()) => 0,
            Err(e) => {
                println!("ERROR {e:#}");
                1
            }
        }
    }

    // ---- sidechain ---------------------------------------------------------------------------

    /// Watches the sidechain level the processors are handed: the loudest value per window.
    fn sidechain_over(meter: &Meter, secs: f32) -> f32 {
        meter.sidechain.take();
        std::thread::sleep(Duration::from_secs_f32(secs));
        meter.sidechain.take()
    }

    /// Time until the level the processors get crosses `threshold` (upwards if `rise`).
    fn sidechain_reaches(meter: &Meter, threshold: f32, rise: bool, secs: f32) -> anyhow::Result<Duration> {
        let t = Instant::now();
        meter.sidechain.take();
        while t.elapsed().as_secs_f32() < secs {
            std::thread::sleep(Duration::from_millis(10));
            let p = meter.sidechain.take();
            if (rise && p >= threshold) || (!rise && p < threshold) {
                return Ok(t.elapsed());
            }
        }
        bail!("the sidechain did not {} {threshold} within {secs} s", if rise { "reach" } else { "fall below" })
    }

    /// The ducking sidechain with the real-world layout: two sources, a game and an ignored app,
    /// each in its own process tree outside this one (the sidechain never listens to
    /// Sidestage's own tree). The apps that were playing before the test are ignored, so the
    /// levels can be checked exactly.
    fn sidechain_checks(
        platform: &mut dyn Platform,
        output: Option<String>,
        mut ignore: Vec<AppKey>,
    ) -> anyhow::Result<()> {
        println!("== sidechain: sources, a game tone and an ignored tone, each in its own process tree");
        if output.is_none() {
            println!("  skipped: no virtual output to play the extra tones on");
            return Ok(());
        }
        const MUSIC: f32 = 0.01;
        const KEPT: f32 = 0.006;
        const IGNORED: f32 = 0.008;
        const GAME: f32 = 0.004;
        let music = OutsideTone::start("win_tone_music", 150, MUSIC)?;
        let music_v = usable_volume(music.pid)?;
        let ignored = OutsideTone::start("win_tone_ignored", 150, IGNORED)?;
        let ignored_v = usable_volume(ignored.pid)?;
        ignore.push(ignored.key());
        let meter = Arc::new(Meter::default());
        let mix = MixMeter::new(&[440.0]);
        let mut s = platform.start(SessionConfig {
            output,
            sidechain: true,
            sidechain_ignore: ignore.clone(),
            sources: vec![source(1, music.key(), HideMethod::Auto, factory(&meter))],
            // Sidestage's own output: a 0.01 tone on the virtual device.
            master: mix.factory(0.01),
        })?;
        let checks = (|| -> anyhow::Result<()> {
            wait_hidden(s.as_ref(), &[1], 5.0)?;
            std::thread::sleep(Duration::from_millis(300));
            let rms = meter.rms_over(1.0);
            println!(
                "  hidden source captured at rms {rms:.6} ({:+.2} dB); its volume {:e}",
                db(rms) - db(MUSIC / std::f32::consts::SQRT_2),
                volume_of(music.pid)?.0
            );
            ensure!(s.status().sidechain_active, "the sidechain is measuring");
            let base = sidechain_over(&meter, 1.5);
            println!(
                "  a hidden {MUSIC} source, an ignored {IGNORED} tone and Sidestage's own 0.01 output: sidechain peak {base:.2e}"
            );
            ensure!(base < 1e-4, "only a hidden source, an ignored app and Sidestage play: the game is silent");

            let game = OutsideTone::start("win_tone_game", 60, GAME)?;
            wait_for_session(game.pid)?;
            let game_before = volume_of(game.pid)?;
            let heard = sidechain_reaches(&meter, GAME * 0.5, true, 3.0)?;
            std::thread::sleep(Duration::from_millis(300));
            let with_game = sidechain_over(&meter, 1.5);
            println!(
                "  a {GAME} game tone (volume {:.3}) was heard {:.0} ms after its stream appeared; sidechain peak {with_game:.5} ({:+.2} dB)",
                game_before.0,
                heard.as_secs_f32() * 1000.0,
                db(with_game) - db(GAME * game_before.0)
            );
            ensure!(
                (db(with_game) - db(GAME * game_before.0)).abs() < 1.0,
                "the sidechain hears the game at its level"
            );
            // Sidestage's own captures get sessions whose meters mirror what they hear; the
            // picker must still trust the game's meter.
            let listed = platform.list_apps()?;
            let game_peak = listed.iter().find(|a| a.key.same_app(&game.key())).and_then(|a| a.peak);
            println!("  list_apps while the sidechain listens to it: the game's peak {game_peak:?}");
            ensure!(
                game_peak.is_some_and(|p| (db(p) - db(GAME)).abs() < 1.0),
                "the picker's meter of an app the sidechain listens to"
            );

            println!("  adding a source that keeps its original audible ({KEPT}, louder than the game)");
            let kept = OutsideTone::start("win_tone_kept", 150, KEPT)?;
            wait_for_session(kept.pid)?;
            s.set_sources(vec![
                source(1, music.key(), HideMethod::Auto, factory(&meter)),
                source(2, kept.key(), HideMethod::KeepOriginal, factory(&Arc::new(Meter::default()))),
            ])?;
            wait_sources(s.as_ref(), &[2], |x| x.state == SessionState::Running, 5.0)?;
            let kept_v = volume_of(kept.pid)?;
            std::thread::sleep(Duration::from_millis(300));
            let with_kept = sidechain_over(&meter, 1.5);
            println!("  sidechain peak {with_kept:.5}: sources never count");
            ensure!((db(with_kept) - db(GAME)).abs() < 1.0, "a source must not reach the sidechain");

            println!("  taking the ignored tone off the ignore list");
            let without: Vec<AppKey> = ignore.iter().filter(|k| **k != ignored.key()).cloned().collect();
            s.set_sidechain_ignore(without)?;
            let took = sidechain_reaches(&meter, IGNORED * 0.8, true, 3.0)?;
            std::thread::sleep(Duration::from_millis(300));
            let unignored = sidechain_over(&meter, 1.5);
            println!(
                "  took effect after {:.0} ms; sidechain peak {unignored:.5} (the {IGNORED} tone counts now)",
                took.as_secs_f32() * 1000.0
            );
            ensure!((db(unignored) - db(IGNORED)).abs() < 1.0, "an app taken off the ignore list counts");
            s.set_sidechain_ignore(ignore.clone())?;
            let took = sidechain_reaches(&meter, IGNORED * 0.75, false, 3.0)?;
            std::thread::sleep(Duration::from_millis(300));
            let reignored = sidechain_over(&meter, 1.5);
            println!(
                "  ignored again: took effect after {:.0} ms; sidechain peak {reignored:.5}",
                took.as_secs_f32() * 1000.0
            );
            ensure!((db(reignored) - db(GAME)).abs() < 1.0, "an ignored app does not count");

            let game_after = volume_of(game.pid)?;
            drop(game);
            let quiet = sidechain_reaches(&meter, 1e-4, false, 3.0)?;
            println!("  the game closed: the sidechain was quiet {:.0} ms later", quiet.as_secs_f32() * 1000.0);

            // The meter path's latency: a game whose stream is open (and listened to) starts a
            // sound at a set moment; how long until the processors are handed its level, next
            // to a bare process-loopback capture of the same game (what any capture of it sees).
            let (mut onsets, mut raw_onsets) = (Vec::new(), Vec::new());
            for _ in 0..5 {
                let at = unix_ms() + 1200;
                let onset = OutsideTone::start_with("win_tone_onset", 30, GAME, &format!("--start-at {at}"))?;
                wait_for_session(onset.pid)?;
                let packets = Arc::new(Mutex::new(Vec::<(Instant, f32)>::with_capacity(1000)));
                let stop = Arc::new(AtomicBool::new(false));
                let raw = {
                    let (packets, stop, pid) = (packets.clone(), stop.clone(), onset.pid);
                    std::thread::spawn(move || raw_capture(pid, &packets, &stop))
                };
                while unix_ms() < at {
                    std::thread::sleep(Duration::from_millis(1));
                }
                let t = Instant::now();
                meter.sidechain.take();
                while t.elapsed() < Duration::from_secs(2) && meter.sidechain.take() < GAME * 0.5 {
                    std::thread::sleep(Duration::from_millis(1));
                }
                let heard = t.elapsed();
                std::thread::sleep(Duration::from_millis(100));
                stop.store(true, Ordering::Relaxed);
                raw.join().map_err(|_| anyhow!("raw capture panicked"))??;
                ensure!(heard < Duration::from_secs(2), "the sidechain did not hear the onset");
                onsets.push(heard.as_secs_f32() * 1000.0);
                let p = packets.lock().map_err(|_| anyhow!("raw capture died"))?;
                if let Some((pt, _)) = p.iter().find(|(pt, pk)| *pt >= t && *pk >= GAME * 0.5) {
                    raw_onsets.push(pt.duration_since(t).as_secs_f32() * 1000.0);
                }
                drop(p);
                drop(onset);
                sidechain_reaches(&meter, 1e-4, false, 3.0)?;
            }
            onsets.sort_by(f32::total_cmp);
            raw_onsets.sort_by(f32::total_cmp);
            println!(
                "  a sound the game starts reaches the processors after {:.0}-{:.0} ms (median {:.0} ms, 5 onsets; includes the game's own output buffer); a bare capture of the game sees it after {:?} ms",
                onsets[0],
                onsets[4],
                onsets[2],
                raw_onsets.iter().map(|x| x.round()).collect::<Vec<_>>()
            );

            // What listening costs: this process's CPU with no app and with five apps heard.
            std::thread::sleep(Duration::from_millis(500));
            let idle = cpu_over(3.0);
            let load: Vec<OutsideTone> = (1..=5)
                .map(|i| OutsideTone::start(&format!("win_tone_load{i}"), 30, 0.002))
                .collect::<anyhow::Result<_>>()?;
            for t in &load {
                wait_for_session(t.pid)?;
            }
            sidechain_reaches(&meter, 0.001, true, 3.0)?;
            std::thread::sleep(Duration::from_millis(500));
            let busy = cpu_over(3.0);
            let heard = sidechain_over(&meter, 1.0);
            println!(
                "  CPU of this process: {idle:.2}% with no app heard, {busy:.2}% with five apps heard (peak {heard:.4})"
            );
            ensure!((db(heard) - db(0.002)).abs() < 1.0, "five 0.002 tones peak at 0.002 (the loudest app)");
            drop(load);
            sidechain_reaches(&meter, 1e-4, false, 3.0)?;
            ensure!(game_before == game_after, "the game's volume was touched");
            ensure!(volume_of(ignored.pid)? == ignored_v, "the ignored app's volume was touched");
            ensure!(volume_of(kept.pid)? == kept_v, "KeepOriginal must not touch the volume");
            print_status(s.as_ref());
            Ok(())
        })();
        let stopped = s.stop();
        let after = volume_of(music.pid)?;
        println!("  source volume after stop {} (before {})", after.0, music_v.0);
        let _ = set_volume_of(music.pid, music_v.0, music_v.1);
        checks?;
        stopped?;
        ensure!((after.0 - music_v.0).abs() < 1e-4 && after.1 == music_v.1, "stop() restores the source");
        Ok(())
    }

    pub fn sidechain_only() -> i32 {
        if unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_err() {
            return 1;
        }
        let r = (|| -> anyhow::Result<()> {
            let mut platform = sidestage_platform::platform()?;
            let output = test_output(platform.as_mut())?;
            let others = other_apps(platform.as_mut())?;
            sidechain_checks(platform.as_mut(), output, others)
        })();
        match r {
            Ok(()) => {
                println!("PASS");
                0
            }
            Err(e) => {
                println!("FAIL: {e:#}");
                1
            }
        }
    }

    /// Three sources for `secs` seconds, reporting latency, drift and dropouts. Sidestage's
    /// output is silence, so playing it on the default device is inaudible; it puts capture and
    /// output on different clocks.
    pub fn soak_only(secs: u64, default_output: bool) -> i32 {
        if unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_err() {
            return 1;
        }
        let r = (|| -> anyhow::Result<()> {
            let voices = Voices(vec![Voice::start(0)?, Voice::start(1)?, Voice::start(2)?]);
            let mut platform = sidestage_platform::platform()?;
            let output = if default_output { None } else { test_output(platform.as_mut())? };
            let others = other_apps(platform.as_mut())?;
            let s = platform.start(SessionConfig {
                output,
                sidechain: true,
                sidechain_ignore: others,
                sources: voices.0.iter().enumerate().map(|(i, v)| v.config(i as SourceId + 1)).collect(),
                master: silent_master(),
            })?;
            let result = wait_hidden(s.as_ref(), &[1, 2, 3], 5.0)
                .and_then(|_| soak(s.as_ref(), &[1, 2, 3], Duration::from_secs(secs)));
            print_status(s.as_ref());
            let stopped = s.stop();
            result.and(stopped)
        })();
        match r {
            Ok(()) => {
                println!("PASS");
                0
            }
            Err(e) => {
                println!("FAIL: {e:#}");
                1
            }
        }
    }

    // ---- volume ramp ---------------------------------------------------------------------------

    /// Measures how long a session-volume change takes to show up in the process-loopback
    /// capture: the backend opens its gate `SETTLE` (200 ms) after hushing, so the new level
    /// must be reached well before that, or the makeup gain would amplify the tail of the
    /// change. Only the test tone's volume is changed.
    pub fn ramp(rounds: usize) -> i32 {
        if unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_err() {
            return 1;
        }
        let r = (|| -> anyhow::Result<()> {
            let tone = Tone::start()?;
            wait_for_session(tone.pid)?;
            let original = volume_of(tone.pid)?;
            let packets = Arc::new(Mutex::new(Vec::<(Instant, f32)>::with_capacity(100 * 60 * 5)));
            let stop = Arc::new(AtomicBool::new(false));
            let cap = {
                let (packets, stop, pid) = (packets.clone(), stop.clone(), tone.pid);
                std::thread::spawn(move || raw_capture(pid, &packets, &stop))
            };
            let result = (|| -> anyhow::Result<()> {
                std::thread::sleep(Duration::from_millis(500));
                let steps: [(f32, f32); 4] = [(1.0, 1e-6), (1e-6, 0.5), (0.5, 1e-6), (1e-6, 1.0)];
                let mut worst = 0f32;
                for round in 0..rounds {
                    for (from, to) in steps {
                        set_volume_of(tone.pid, from, false)?;
                        std::thread::sleep(Duration::from_millis(500));
                        let t0 = Instant::now();
                        set_volume_of(tone.pid, to, false)?;
                        std::thread::sleep(Duration::from_millis(500));
                        let p = packets.lock().map_err(|_| anyhow!("capture thread died"))?;
                        let window: Vec<(f32, f32)> = p
                            .iter()
                            .filter(|(t, _)| *t >= t0)
                            .map(|(t, peak)| (t.duration_since(t0).as_secs_f32() * 1000.0, *peak))
                            .collect();
                        drop(p);
                        let (start, target) = (tone.amp * from, tone.amp * to);
                        let moved = window.iter().find(|(_, pk)| (pk - start).abs() > start * 0.01).map(|w| w.0);
                        // First packet from which every later one is within 1 % of the target.
                        let settled = (0..window.len())
                            .find(|&i| window[i..].iter().all(|(_, pk)| (pk - target).abs() <= target * 0.01))
                            .map(|i| window[i].0);
                        let Some(settled) = settled else {
                            bail!("{from:e} -> {to:e}: the capture never reached the new level");
                        };
                        worst = worst.max(settled);
                        println!(
                            "  round {round}: {from:e} -> {to:e}: first change at {:.0} ms, settled at {settled:.0} ms",
                            moved.unwrap_or(f32::NAN)
                        );
                    }
                }
                println!("  slowest change reached the capture after {worst:.0} ms (SETTLE is 200 ms)");
                ensure!(worst < 150.0, "volume changes take too long to reach the capture");
                Ok(())
            })();
            stop.store(true, Ordering::Relaxed);
            let cap = cap.join().map_err(|_| anyhow!("capture thread panicked"))?;
            set_volume_of(tone.pid, original.0, original.1)?;
            println!("  tone volume restored to {:?}", volume_of(tone.pid)?);
            result.and(cap)
        })();
        match r {
            Ok(()) => {
                println!("PASS");
                0
            }
            Err(e) => {
                println!("FAIL: {e:#}");
                1
            }
        }
    }

    /// A bare process-loopback capture of `pid` that records each packet's arrival time and
    /// peak.
    fn raw_capture(pid: u32, packets: &Mutex<Vec<(Instant, f32)>>, stop: &AtomicBool) -> anyhow::Result<()> {
        use std::mem::ManuallyDrop;
        use windows::Win32::Media::Audio::{
            AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            AUDCLNT_STREAMFLAGS_LOOPBACK, AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
            AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
            ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
            IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
            IAudioCaptureClient, IAudioClient, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX,
        };
        use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
        use windows::Win32::System::Com::BLOB;
        use windows::Win32::System::Com::StructuredStorage::{
            PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
        };
        use windows::Win32::System::Threading::CreateEventW;
        use windows::Win32::System::Variant::VT_BLOB;
        use windows::core::{HRESULT, IUnknown, Ref, implement};

        #[implement(IActivateAudioInterfaceCompletionHandler)]
        struct Done(mpsc::SyncSender<()>);
        impl IActivateAudioInterfaceCompletionHandler_Impl for Done_Impl {
            fn ActivateCompleted(&self, _op: Ref<IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
                let _ = self.0.try_send(());
                Ok(())
            }
        }

        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
        let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        };
        let prop = ManuallyDrop::new(PROPVARIANT {
            Anonymous: PROPVARIANT_0 {
                Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                    vt: VT_BLOB,
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: PROPVARIANT_0_0_0 {
                        blob: BLOB {
                            cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                            pBlobData: (&raw mut params).cast(),
                        },
                    },
                }),
            },
        });
        let (tx, rx) = mpsc::sync_channel(1);
        let handler: IActivateAudioInterfaceCompletionHandler = Done(tx).into();
        let op = unsafe {
            ActivateAudioInterfaceAsync(
                VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
                &IAudioClient::IID,
                Some(&*prop as *const PROPVARIANT),
                &handler,
            )
        }?;
        rx.recv_timeout(Duration::from_secs(5))?;
        let (mut hr, mut unk) = (HRESULT(0), None::<IUnknown>);
        unsafe { op.GetActivateResult(&mut hr, &mut unk) }?;
        hr.ok()?;
        let client: IAudioClient = unk.context("no client")?.cast()?;
        let fmt = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 48_000 * 8,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        let flags =
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM;
        unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 200_000, 0, &fmt, None) }?;
        let event = OwnedHandle(unsafe { CreateEventW(None, false, false, None) }?);
        unsafe { client.SetEventHandle(event.raw()) }?;
        let cap: IAudioCaptureClient = unsafe { client.GetService() }?;
        unsafe { client.Start() }?;
        while !stop.load(Ordering::Relaxed) {
            unsafe { WaitForSingleObject(event.raw(), 200) };
            loop {
                let n = unsafe { cap.GetNextPacketSize() }?;
                if n == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let (mut frames, mut fl) = (0u32, 0u32);
                unsafe { cap.GetBuffer(&mut data, &mut frames, &mut fl, None, None) }?;
                let s = unsafe { std::slice::from_raw_parts(data as *const f32, frames as usize * 2) };
                let peak = sidestage_platform::peak(s);
                unsafe { cap.ReleaseBuffer(frames) }?;
                if let Ok(mut p) = packets.lock() {
                    p.push((Instant::now(), peak));
                }
            }
        }
        unsafe { client.Stop() }?;
        Ok(())
    }
}
