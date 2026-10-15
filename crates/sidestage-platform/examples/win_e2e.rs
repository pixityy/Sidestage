//! Live end-to-end test of the Windows backend.
//!
//! Safe by design: the only process whose volume, mute or routing this test changes (directly
//! or through Sidestage) is the quiet test tone it starts itself (`win_tone`). Every other app
//! is left alone, and the tone's volume is put back even when a check fails.
//!
//! What it checks:
//! 1. `list_apps` finds the tone, and how long it takes.
//! 2. KeepOriginal: the capture matches the tone and its volume is untouched.
//! 3. Hide (Auto): the tone's session sits at ~1e-6 while the captured level still matches the
//!    tone within 1 dB (makeup gain), and `stop()` restores the volume.
//! 4. A foreign volume change (as if the user moved the mixer slider) is adopted and re-hushed;
//!    killing and restarting the tone re-attaches; `stop()` restores the adopted volume.
//! 5. Another instance of the app is left alone, except that a session of it that Windows
//!    started at the hushed volume gets the app's volume back (a deliberate 0 stays 0).
//! 6. Crash: a child process runs a hiding session and is killed together with its whole
//!    process tree (Task Manager's "End process tree"); the guardian, which is not in that tree,
//!    must restore the tone's volume within a few seconds.
//! 7. `recover()` then finds nothing left to restore.
//! 8. Crash while the app is closed: `recover()` keeps the entry, and the volume comes back the
//!    moment the app opens audio again.
//! 9. The same without `recover()`: the dead run's guardian restores the volume the moment
//!    the app opens audio (it is woken by the new session, not by its polling).
//!
//! The sidechain checks start two more tones as children of the shell (outside this process
//! tree, like a real music app and game) on the virtual output: the hushed "music" and
//! Sidestage's own output must not reach the sidechain, the "game" must, at its level.
//!
//! ```text
//! cargo build -p sidestage-platform --examples
//! target\debug\examples\win_e2e.exe            # all checks
//! target\debug\examples\win_e2e.exe --soak 120 # hide the tone for 120 s, report drift/dropouts
//! target\debug\examples\win_e2e.exe --soak 120 --default-output # same, output on another clock
//! target\debug\examples\win_e2e.exe --sidechain 600 # sidechain only, waiting for quiet
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
    let code = match args.first().map(String::as_str) {
        Some("--child-session") => e2e::child_session(&args[1..]),
        Some("--sidechain") => e2e::sidechain_only(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(600)),
        Some("--soak") => e2e::soak(
            args.get(1).and_then(|s| s.parse().ok()).unwrap_or(60),
            args.iter().any(|a| a == "--default-output"),
        ),
        Some("--ramp") => e2e::ramp(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(5)),
        _ => e2e::run(),
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use anyhow::{Context, anyhow, bail, ensure};
    use sidestage_platform::{
        AppKey, AudioApp, HideMethod, Platform, Processor, ProcessorFactory, Session, SessionConfig, SessionState,
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

    // ---- the test tone -----------------------------------------------------------------------

    struct Tone {
        child: Child,
        pid: u32,
        amp: f32,
        path: String,
    }

    impl Tone {
        fn start() -> anyhow::Result<Tone> {
            Self::start_with(&[])
        }

        fn start_with(extra: &[&str]) -> anyhow::Result<Tone> {
            let exe = std::env::current_exe()?.with_file_name("win_tone.exe");
            ensure!(exe.exists(), "{} not found; run `cargo build -p sidestage-platform --examples`", exe.display());
            let mut child = Command::new(&exe)
                .args(["--seconds", "600"])
                .args(extra)
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .context("starting win_tone")?;
            let mut line = String::new();
            BufReader::new(child.stdout.take().context("tone stdout")?).read_line(&mut line)?;
            let field = |k: &str| {
                line.split_whitespace().find_map(|w| w.strip_prefix(k)).map(str::to_owned).unwrap_or_default()
            };
            let amp: f32 = field("amp=").parse().context("tone amplitude")?;
            let device = line.split("device=").nth(1).unwrap_or("?").trim().to_owned();
            ensure!(amp <= 0.01, "tone too loud");
            println!("  tone: pid {} amp {amp} on '{device}'", child.id());
            Ok(Tone { pid: child.id(), child, amp, path: exe.to_string_lossy().into_owned() })
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
    /// real music app or game: outside Sidestage's tree, so the sidechain hears it unless it is
    /// hushed. It runs a renamed copy of `win_tone.exe`, so its volume is separate from the
    /// other tones', and plays on the virtual output (amplitude 0.01).
    struct OutsideTone {
        process: HANDLE,
        pid: u32,
        path: String,
    }

    impl OutsideTone {
        fn start(name: &str, seconds: u64) -> anyhow::Result<OutsideTone> {
            use windows::Win32::System::Threading::{
                CREATE_NO_WINDOW, CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
                InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_PARENT_PROCESS,
                PROCESS_INFORMATION, STARTUPINFOEXW, STARTUPINFOW, UpdateProcThreadAttribute,
            };
            let src = std::env::current_exe()?.with_file_name("win_tone.exe");
            let exe = src.with_file_name(format!("{name}.exe"));
            // A copy left running by an aborted run keeps the file locked; reuse it then.
            if let Err(e) = std::fs::copy(&src, &exe) {
                ensure!(exe.exists(), "copying {}: {e}", src.display());
            }
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
                    format!("\"{}\" --seconds {seconds}", exe.display()).encode_utf16().chain([0]).collect();
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
            println!("  outside tone '{name}': pid {}", pi.dwProcessId);
            Ok(OutsideTone { process: pi.hProcess, pid: pi.dwProcessId, path: exe.to_string_lossy().into_owned() })
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

    // ---- reading and resetting the tone's session volume (only ever the tone's) --------------

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

    // ---- a Processor that outputs silence and measures its input ------------------------------

    #[derive(Default)]
    struct Meter {
        sumsq: AtomicU64,
        frames: AtomicU64,
        blocks: AtomicU64,
        /// Loudest sidechain level the processor was handed.
        sidechain: sidestage_platform::AtomicLevel,
    }

    struct Recorder {
        meter: Arc<Meter>,
        sumsq: f64,
        frames: u64,
        /// Amplitude of a 440 Hz tone written to the output (0 = silence). Only ever non-zero
        /// when the output is the virtual device nobody listens to.
        emit: f32,
        phase: f32,
        step: f32,
    }

    impl Processor for Recorder {
        fn process(&mut self, input: &[f32], output: &mut [f32], sidechain_peak: f32) {
            if self.emit > 0.0 {
                for f in output.as_chunks_mut::<2>().0 {
                    let s = (self.phase * std::f32::consts::TAU).sin() * self.emit;
                    self.phase = (self.phase + self.step).fract();
                    *f = [s, s];
                }
            } else {
                output.fill(0.0);
            }
            self.meter.sidechain.raise(sidechain_peak);
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

    fn factory(meter: &Arc<Meter>) -> ProcessorFactory {
        emitting_factory(meter, 0.0)
    }

    fn emitting_factory(meter: &Arc<Meter>, emit: f32) -> ProcessorFactory {
        assert!(emit <= 0.02, "test output must stay quiet");
        let meter = meter.clone();
        Arc::new(move |rate| {
            Box::new(Recorder {
                meter: meter.clone(),
                sumsq: 0.0,
                frames: 0,
                emit,
                phase: 0.0,
                step: 440.0 / rate as f32,
            }) as Box<dyn Processor>
        })
    }

    fn db(x: f32) -> f32 {
        20.0 * x.max(1e-12).log10()
    }

    fn wait_state(s: &dyn Session, want: impl Fn(&SessionState, bool) -> bool, secs: f32) -> anyhow::Result<f32> {
        let t = Instant::now();
        loop {
            let st = s.status();
            if want(&st.state, st.original_hidden) {
                return Ok(t.elapsed().as_secs_f32());
            }
            if let SessionState::Error(e) = &st.state {
                bail!("session error: {e}");
            }
            if t.elapsed().as_secs_f32() > secs {
                bail!("timed out in state {:?} (hidden {}), warnings {:?}", st.state, st.original_hidden, st.warnings);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn print_status(s: &dyn Session) {
        let st = s.status();
        println!(
            "  status: {:?} app='{}' out='{}' {} Hz hidden={} latency={:.1} ms drift={:.1} ppm dropouts={} capture_peak={:.5} sidechain_peak={:.4}",
            st.state,
            st.app_name,
            st.output_name,
            st.sample_rate,
            st.original_hidden,
            st.latency_ms,
            st.drift_ppm,
            st.dropouts,
            st.capture_peak,
            st.sidechain_peak
        );
        for w in &st.warnings {
            println!("  warning: {w}");
        }
    }

    struct Ctx {
        platform: Box<dyn Platform>,
        key: AppKey,
        output: Option<String>,
        tone: Tone,
        /// The tone's volume and mute before the test; restored at the end no matter what.
        original: (f32, bool),
    }

    pub fn run() -> i32 {
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
            wait_for_session(pid)?;
            println!("  the tone has {} audio session(s)", sessions_of(pid)?.len());
            let (mut v, m) = volume_of(pid)?;
            if v < 0.01 {
                println!("  the tone's saved volume is {v:e} (left over from an aborted run); resetting it to 1.0");
                set_volume_of(pid, 1.0, false)?;
                v = 1.0;
            }
            original = Some((v, m));
            let mut ctx = setup(tone, (v, m))?;
            let r = all_checks(&mut ctx);
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
        let output = outputs.iter().find(|o| o.name.contains("Steam Streaming Speakers")).map(|o| o.id.clone());

        println!("== list_apps");
        let key = AppKey(tone.path.to_lowercase());
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
        Ok(Ctx { platform, key, output, tone, original })
    }

    fn config(ctx: &Ctx, hide: HideMethod) -> SessionConfig {
        SessionConfig { app: ctx.key.clone(), output: ctx.output.clone(), hide, sidechain: true }
    }

    fn all_checks(ctx: &mut Ctx) -> anyhow::Result<()> {
        let expect = ctx.tone.amp / std::f32::consts::SQRT_2;
        let (orig_v, _) = ctx.original;

        let me = AppKey(std::env::current_exe()?.to_string_lossy().to_lowercase());
        let own = ctx.platform.start(
            SessionConfig { app: me, output: None, hide: HideMethod::Auto, sidechain: false },
            factory(&Arc::new(Meter::default())),
        );
        match own {
            Ok(_) => bail!("capturing Sidestage itself must be refused"),
            Err(e) => println!("== start() on Sidestage itself is refused: {e:#}"),
        }

        // -- 2. KeepOriginal ---------------------------------------------------------------
        println!("== KeepOriginal");
        let meter = Arc::new(Meter::default());
        let t = Instant::now();
        let s = ctx.platform.start(config(ctx, HideMethod::KeepOriginal), factory(&meter))?;
        println!("  start() returned in {:.1} ms", t.elapsed().as_secs_f32() * 1000.0);
        wait_state(s.as_ref(), |st, _| *st == SessionState::Running, 5.0)?;
        std::thread::sleep(Duration::from_millis(300));
        let rms = meter.rms_over(1.5);
        let (v, _) = volume_of(ctx.tone.pid)?;
        println!("  captured rms {rms:.6} (expected {expect:.6}, {:+.2} dB), tone volume {v}", db(rms) - db(expect));
        print_status(s.as_ref());
        ensure!((db(rms) - db(expect)).abs() < 1.0, "KeepOriginal capture level is off");
        ensure!((v - orig_v).abs() < 1e-6, "KeepOriginal must not change the volume");
        ensure!(!s.status().original_hidden);
        s.stop()?;

        // -- 3. Hide ----------------------------------------------------------------------
        println!("== Hide (Auto)");
        let meter = Arc::new(Meter::default());
        let t = Instant::now();
        let s = ctx.platform.start(config(ctx, HideMethod::Auto), factory(&meter))?;
        println!("  start() returned in {:.1} ms", t.elapsed().as_secs_f32() * 1000.0);
        wait_state(s.as_ref(), |st, hidden| *st == SessionState::Running && hidden, 5.0)?;
        println!("  running and hidden {:.0} ms after start()", t.elapsed().as_secs_f32() * 1000.0);
        std::thread::sleep(Duration::from_millis(300));
        let rms = meter.rms_over(2.0);
        let (v, m) = volume_of(ctx.tone.pid)?;
        println!(
            "  captured rms {rms:.6} (expected {expect:.6}, {:+.3} dB); tone session volume {v:e} muted={m}",
            db(rms) - db(expect)
        );
        println!(
            "  loudest sidechain level handed to the processor: {:.4} (everything except Sidestage's tree)",
            meter.sidechain.take()
        );
        print_status(s.as_ref());
        ensure!((db(rms) - db(expect)).abs() < 1.0, "makeup gain does not restore the tone's level within 1 dB");
        ensure!((0.5e-6..=2e-6).contains(&v), "the tone's session volume should be ~1e-6 while hidden");
        ensure!(!m, "hushed, not muted");
        let st = s.status();
        ensure!(st.original_hidden && st.sample_rate > 0 && st.latency_ms > 0.0);
        let t = Instant::now();
        s.stop()?;
        let (v, m) = volume_of(ctx.tone.pid)?;
        println!("  stop() took {:.1} ms; tone volume after stop {v} muted={m}", t.elapsed().as_secs_f32() * 1000.0);
        ensure!((v - orig_v).abs() < 1e-4, "stop() must restore the original volume");

        sidechain_checks(ctx.platform.as_mut(), ctx.output.clone(), Duration::ZERO)?;

        println!("== dropping a hiding session without stop()");
        let meter = Arc::new(Meter::default());
        let s = ctx.platform.start(config(ctx, HideMethod::Auto), factory(&meter))?;
        wait_state(s.as_ref(), |st, hidden| *st == SessionState::Running && hidden, 5.0)?;
        ensure!(volume_of(ctx.tone.pid)?.0 < 1e-5, "hushed");
        drop(s);
        let (v, _) = volume_of(ctx.tone.pid)?;
        println!("  tone volume after drop {v}");
        ensure!((v - orig_v).abs() < 1e-4, "Drop must restore the original volume");

        // -- 3c. The app opens audio after Sidestage attached --------------------------------
        println!("== the app opens its audio only after Sidestage attached");
        ctx.tone.kill();
        ctx.tone = Tone::start_with(&["--delay-ms", "2500"])?;
        let meter = Arc::new(Meter::default());
        let s = ctx.platform.start(config(ctx, HideMethod::Auto), factory(&meter))?;
        std::thread::sleep(Duration::from_millis(700));
        let st = s.status();
        println!("  before the app has a stream: {:?}, hidden={}", st.state, st.original_hidden);
        ensure!(
            st.state == SessionState::WaitingForApp && sessions_of(ctx.tone.pid)?.is_empty(),
            "a running app without an audio stream is 'waiting'"
        );
        wait_for_session(ctx.tone.pid)?;
        let t = Instant::now();
        wait_state(s.as_ref(), |st, hidden| *st == SessionState::Running && hidden, 5.0)?;
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

        // -- 4. Foreign change, restart, re-attach ------------------------------------------
        println!("== foreign volume change and re-attach");
        let meter = Arc::new(Meter::default());
        let s = ctx.platform.start(config(ctx, HideMethod::Auto), factory(&meter))?;
        wait_state(s.as_ref(), |st, hidden| *st == SessionState::Running && hidden, 5.0)?;
        std::thread::sleep(Duration::from_millis(300));
        println!("  setting the tone's volume to 0.5 from outside Sidestage");
        set_volume_of(ctx.tone.pid, 0.5, false)?;
        let t = Instant::now();
        let mut rehushed = None;
        while t.elapsed() < Duration::from_secs(3) {
            let (v, _) = volume_of(ctx.tone.pid)?;
            if v < 1e-5 && s.status().state == SessionState::Running && meter.blocks.load(Ordering::Relaxed) > 0 {
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
        let st = s.status();
        println!(
            "  after the drag: captured rms {rms:.6} ({:+.3} dB vs 0.5x), warnings {:?}",
            db(rms) - db(want),
            st.warnings
        );
        ensure!((db(rms) - db(want)).abs() < 1.0, "level after the drag");
        ensure!(!st.warnings.iter().any(|w| w.contains("keeps changing")), "a drag is not a fight");

        println!("  an app that resets its volume right after every hush");
        // The first change comes long after the last hush; the next three are fights.
        for _ in 0..4 {
            wait_hushed(ctx.tone.pid, 5.0)?;
            set_volume_of(ctx.tone.pid, 0.5, false)?;
        }
        std::thread::sleep(Duration::from_millis(100));
        let st = s.status();
        println!("  warnings: {:?}", st.warnings);
        ensure!(st.warnings.iter().any(|w| w.contains("keeps changing")), "fighting is reported");
        wait_hushed(ctx.tone.pid, 5.0)?;

        println!("  restarting the tone");
        ctx.tone.kill();
        wait_state(s.as_ref(), |st, _| *st == SessionState::WaitingForApp, 5.0)?;
        println!("  state WaitingForApp");
        ctx.tone = Tone::start()?;
        wait_for_session(ctx.tone.pid)?;
        let took = wait_state(s.as_ref(), |st, hidden| *st == SessionState::Running && hidden, 8.0)?;
        std::thread::sleep(Duration::from_millis(500));
        let rms = meter.rms_over(1.5);
        let (v, _) = volume_of(ctx.tone.pid)?;
        println!(
            "  re-attached after {:.0} ms; tone volume {v:e}; captured rms {rms:.6} (expected {want:.6}, {:+.3} dB)",
            took * 1000.0,
            db(rms) - db(want)
        );
        print_status(s.as_ref());
        ensure!((0.5e-6..=2e-6).contains(&v), "the restarted tone must be hushed");
        ensure!((db(rms) - db(want)).abs() < 1.0, "level after re-attach");
        s.stop()?;
        let (v, _) = volume_of(ctx.tone.pid)?;
        println!("  after stop: tone volume {v} (the adopted 0.5)");
        ensure!((v - 0.5).abs() < 1e-4, "stop() restores the adopted volume");
        set_volume_of(ctx.tone.pid, ctx.original.0, ctx.original.1)?;

        // -- 5. Another instance -----------------------------------------------------------
        other_instance(ctx)?;

        // -- 6. Crash -----------------------------------------------------------------------
        println!("== crash: end a hiding Sidestage process and its whole process tree");
        let mut child = spawn_child_session(ctx)?;
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
        let t = Instant::now();
        let mut restored = None;
        while t.elapsed() < Duration::from_secs(10) {
            let (v, _) = volume_of(ctx.tone.pid)?;
            if (v - orig_v).abs() < 1e-4 {
                restored = Some(t.elapsed());
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let restored = restored.context("the guardian did not restore the tone's volume within 10 s")?;
        println!("  guardian restored the volume {:.0} ms after the kill", restored.as_secs_f32() * 1000.0);
        ensure!(restored < Duration::from_secs(5), "restore must happen within a few seconds");

        // -- 7. recover ---------------------------------------------------------------------
        println!("== recover");
        std::thread::sleep(Duration::from_millis(300));
        let lines = ctx.platform.recover()?;
        println!("  recover(): {lines:?}");
        ensure!(lines.is_empty(), "nothing should be left to recover");
        let journal = sidestage_platform::journal::state_dir()?.join("windows-hush.json");
        ensure!(!journal.exists(), "the journal is cleared");

        // -- 8. Crash while the app is closed -------------------------------------------------
        println!("== crash while the app is closed; recover(); restore when it plays again");
        let mut child = spawn_child_session(ctx)?;
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
        let t = Instant::now();
        let mut restored = None;
        while t.elapsed() < Duration::from_secs(8) {
            let (v, _) = volume_of(ctx.tone.pid)?;
            if (v - orig_v).abs() < 1e-4 {
                restored = Some(t.elapsed());
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let restored = restored.context("the relaunched tone was not restored")?;
        println!(
            "  relaunched tone came up at volume {v0:e}; restored {:.0} ms later",
            restored.as_secs_f32() * 1000.0
        );
        std::thread::sleep(Duration::from_millis(300));
        let lines = ctx.platform.recover()?;
        ensure!(lines.is_empty() && !journal.exists(), "nothing left after the restore");

        // -- 9. The guardian alone --------------------------------------------------------
        // This process has nothing pending now, so it leaves the journal to the dead run's
        // guardian.
        println!("== crash while the app is closed; the guardian restores it when it plays");
        let mut child = spawn_child_session(ctx)?;
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

    /// A second instance of the music app is not captured. It keeps its own volume, except
    /// when Windows starts it at the hushed volume (Windows gives a new session the volume of
    /// the app's last session that closed); then it gets the app's volume back.
    fn other_instance(ctx: &mut Ctx) -> anyhow::Result<()> {
        println!("== another instance of the app");
        let (orig_v, _) = ctx.original;
        let expect = ctx.tone.amp / std::f32::consts::SQRT_2;
        let meter = Arc::new(Meter::default());
        let s = ctx.platform.start(config(ctx, HideMethod::Auto), factory(&meter))?;
        let r = (|| -> anyhow::Result<()> {
            wait_state(s.as_ref(), |st, hidden| *st == SessionState::Running && hidden, 5.0)?;
            let second = Tone::start()?;
            wait_for_session(second.pid)?;
            std::thread::sleep(Duration::from_millis(1500));
            let (v1, v2) = (volume_of(ctx.tone.pid)?.0, volume_of(second.pid)?.0);
            println!("  first instance volume {v1:e} (captured), second {v2}");
            ensure!(v1 < 1e-5, "the captured instance stays hushed");
            ensure!((v2 - orig_v).abs() < 1e-4, "the second instance is left alone");
            let warnings = s.status().warnings;
            println!("  warnings: {warnings:?}");
            ensure!(warnings.iter().any(|w| w.contains("More than one")), "the uncaptured instance is pointed out");
            set_volume_of(second.pid, 1e-6, false)?;
            let t = Instant::now();
            let mut back = None;
            while t.elapsed() < Duration::from_secs(4) {
                if (volume_of(second.pid)?.0 - orig_v).abs() < 1e-4 {
                    back = Some(t.elapsed());
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let back = back.context("the second instance stayed at the hushed volume")?;
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

    /// The ducking sidechain with the real-world layout: music and "game" each in their own
    /// process tree. While the music is hushed and Sidestage itself plays a tone, the sidechain
    /// must hear (almost) nothing; a game tone must show up at its own level.
    ///
    /// The sidechain is everything except Sidestage, so the user's own apps (music playing in
    /// Spotify) are in it too; the exact levels are only checked once the baseline is quiet,
    /// waiting up to `quiet_wait` for that.
    fn sidechain_checks(
        platform: &mut dyn Platform,
        output: Option<String>,
        quiet_wait: Duration,
    ) -> anyhow::Result<()> {
        println!("== sidechain: music and game tones in their own process trees");
        if output.is_none() {
            println!("  skipped: no virtual output to play the extra tones on");
            return Ok(());
        }
        const AMP: f32 = 0.01;
        let tone_secs = quiet_wait.as_secs() + 90;
        let music = OutsideTone::start("win_tone_music", tone_secs)?;
        wait_for_session(music.pid)?;
        if volume_of(music.pid)?.0 < 0.01 {
            set_volume_of(music.pid, 1.0, false)?;
        }
        let (music_v, music_m) = volume_of(music.pid)?;
        let meter = Arc::new(Meter::default());
        let s = platform.start(
            SessionConfig { app: AppKey(music.path.to_lowercase()), output, hide: HideMethod::Auto, sidechain: true },
            // Sidestage's own output: a 0.02 tone on the virtual device.
            emitting_factory(&meter, 0.02),
        )?;
        let checks = (|| -> anyhow::Result<()> {
            wait_state(s.as_ref(), |st, hidden| *st == SessionState::Running && hidden, 5.0)?;
            std::thread::sleep(Duration::from_millis(300));
            let rms = meter.rms_over(1.5);
            let expect = AMP / std::f32::consts::SQRT_2;
            let waited = Instant::now();
            let mut said = Instant::now();
            let base = loop {
                meter.sidechain.take();
                std::thread::sleep(Duration::from_secs(1));
                let base = meter.sidechain.take();
                if base < 1e-3 || waited.elapsed() >= quiet_wait {
                    break base;
                }
                if said.elapsed() >= Duration::from_secs(30) {
                    println!("  waiting for other apps to go quiet (sidechain peak {base:.3})");
                    said = Instant::now();
                }
            };
            println!(
                "  music hidden: captured rms {rms:.6} ({:+.3} dB), its volume {:e}",
                db(rms) - db(expect),
                volume_of(music.pid)?.0
            );
            println!("  sidechain while only the hushed music and Sidestage's own 0.02 output play: peak {base:.2e}");
            ensure!((db(rms) - db(expect)).abs() < 1.0, "hidden music level is off");
            let quiet = base < 1e-3;
            if !quiet {
                println!("  note: other apps are making sound; the exact sidechain levels are not checked");
            }

            let game = OutsideTone::start("win_tone_game", 60)?;
            wait_for_session(game.pid)?;
            let game_before = volume_of(game.pid)?;
            std::thread::sleep(Duration::from_millis(300));
            meter.sidechain.take();
            std::thread::sleep(Duration::from_millis(1500));
            let with_game = meter.sidechain.take();
            let game_after = volume_of(game.pid)?;
            println!(
                "  sidechain with a {AMP} game tone (volume {:.3}): peak {with_game:.5} ({:+.2} dB)",
                game_before.0,
                db(with_game) - db(AMP * game_before.0)
            );
            drop(game);
            ensure!(game_before == game_after, "the game's volume was touched");
            if quiet {
                ensure!(
                    (db(with_game) - db(AMP * game_before.0)).abs() < 1.0,
                    "the sidechain does not hear the game at its level"
                );
            }
            print_status(s.as_ref());
            Ok(())
        })();
        let stopped = s.stop();
        let after = volume_of(music.pid)?;
        println!("  music volume after stop {} (before {music_v})", after.0);
        checks?;
        stopped?;
        ensure!((after.0 - music_v).abs() < 1e-4 && after.1 == music_m, "stop() must restore the music's volume");
        Ok(())
    }

    /// Starts `--child-session` and waits until it reports that the tone is hidden.
    fn spawn_child_session(ctx: &Ctx) -> anyhow::Result<Child> {
        let exe = std::env::current_exe()?;
        let mut args = vec!["--child-session".to_owned(), ctx.key.0.clone()];
        if let Some(o) = &ctx.output {
            args.push(o.clone());
        }
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

    /// Hides the tone for `secs` seconds and reports latency, drift and dropouts over time.
    /// Sidestage's output is silence (the recording processor), so playing it on the default
    /// device is inaudible; it puts capture and output on different clocks.
    pub fn soak(secs: u64, default_output: bool) -> i32 {
        if unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_err() {
            return 1;
        }
        let r = (|| -> anyhow::Result<()> {
            let tone = Tone::start()?;
            wait_for_session(tone.pid)?;
            let original = volume_of(tone.pid)?;
            let mut platform = sidestage_platform::platform()?;
            let output = platform
                .list_outputs()?
                .into_iter()
                .find(|o| !default_output && o.name.contains("Steam Streaming Speakers"))
                .map(|o| o.id);
            let meter = Arc::new(Meter::default());
            let cfg = SessionConfig {
                app: AppKey(tone.path.to_lowercase()),
                output,
                hide: HideMethod::Auto,
                sidechain: true,
            };
            let s = platform.start(cfg, factory(&meter))?;
            let result = (|| -> anyhow::Result<()> {
                wait_state(s.as_ref(), |st, hidden| *st == SessionState::Running && hidden, 5.0)?;
                let expect = tone.amp / std::f32::consts::SQRT_2;
                let t = Instant::now();
                while t.elapsed() < Duration::from_secs(secs) {
                    let rms = meter.rms_over(10.0);
                    let st = s.status();
                    println!(
                        "  t={:4.0}s level {:+.3} dB latency {:.1} ms drift {:+.1} ppm dropouts {} state {:?}",
                        t.elapsed().as_secs_f32(),
                        db(rms) - db(expect),
                        st.latency_ms,
                        st.drift_ppm,
                        st.dropouts,
                        st.state
                    );
                }
                Ok(())
            })();
            let stopped = s.stop();
            let after = volume_of(tone.pid)?;
            println!("  volume before {:?}, after stop {:?}", original, after);
            set_volume_of(tone.pid, original.0, original.1)?;
            result.and(stopped)
        })();
        match r {
            Ok(()) => 0,
            Err(e) => {
                println!("FAIL: {e:#}");
                1
            }
        }
    }

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
            let packets = Arc::new(std::sync::Mutex::new(Vec::<(Instant, f32)>::with_capacity(100 * 60 * 5)));
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
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
    fn raw_capture(
        pid: u32,
        packets: &std::sync::Mutex<Vec<(Instant, f32)>>,
        stop: &std::sync::atomic::AtomicBool,
    ) -> anyhow::Result<()> {
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

    /// Only the sidechain checks, waiting up to `wait_secs` for the user's apps to go quiet so
    /// the exact levels can be checked.
    pub fn sidechain_only(wait_secs: u64) -> i32 {
        if unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_err() {
            return 1;
        }
        let r = (|| -> anyhow::Result<()> {
            let mut platform = sidestage_platform::platform()?;
            let output = platform
                .list_outputs()?
                .into_iter()
                .find(|o| o.name.contains("Steam Streaming Speakers"))
                .map(|o| o.id);
            sidechain_checks(platform.as_mut(), output, Duration::from_secs(wait_secs))
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

    /// Child mode for the crash test: start a hiding session, report, then wait to be killed.
    pub fn child_session(args: &[String]) -> i32 {
        let r = (|| -> anyhow::Result<()> {
            let key = AppKey(args.first().context("app key")?.clone());
            let output = args.get(1).cloned();
            let mut platform = sidestage_platform::platform()?;
            let meter = Arc::new(Meter::default());
            let s = platform
                .start(SessionConfig { app: key, output, hide: HideMethod::Auto, sidechain: false }, factory(&meter))?;
            wait_state(s.as_ref(), |st, hidden| *st == SessionState::Running && hidden, 5.0)?;
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
}
