//! Read-only diagnostic for the ducking sidechain: traces the peak meter of every audio session
//! (what the Volume mixer's green bars show) next to its endpoint's meter, and times the calls.
//! Changes nothing.
//!
//! This is how the measurement behind the Windows backend's sidechain design was made: session
//! meters show what a session renders before its volume, except that a process running a
//! process-loopback capture gets a session whose meter mirrors the captured audio (and some
//! drivers report the endpoint's whole mix on every session). Both show up here as "IDENTICAL"
//! lines, which is why the sidechain listens with its own captures instead of reading meters.
//!
//! ```text
//! cargo run -p sidestage-platform --example win_meters -- 3     # seconds, default 3
//! ```

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use std::time::{Duration, Instant};

    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::Foundation::S_OK;
    use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
    use windows::Win32::Media::Audio::{
        DEVICE_STATE_ACTIVE, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator, ISimpleAudioVolume,
        MMDeviceEnumerator, eRender,
    };
    use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, STGM_READ};
    use windows::core::Interface;

    struct S {
        ep: usize,
        pid: u32,
        meter: IAudioMeterInformation,
        vol: ISimpleAudioVolume,
        trace: Vec<f32>,
    }

    if let Some(code) = sidestage_platform::run_helper_if_requested() {
        std::process::exit(code);
    }
    let secs: f32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(3.0);
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
    let en: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }?;
    let devs = unsafe { en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }?;
    let mut eps: Vec<(String, IAudioMeterInformation, Vec<f32>)> = Vec::new();
    let mut ss: Vec<S> = Vec::new();
    for i in 0..unsafe { devs.GetCount() }? {
        let dev = unsafe { devs.Item(i) }?;
        let name = unsafe { dev.OpenPropertyStore(STGM_READ)?.GetValue(&PKEY_Device_FriendlyName)?.to_string() };
        let ep = eps.len();
        eps.push((name, unsafe { dev.Activate(CLSCTX_ALL, None) }?, Vec::new()));
        let mgr: IAudioSessionManager2 = unsafe { dev.Activate(CLSCTX_ALL, None) }?;
        let se = unsafe { mgr.GetSessionEnumerator() }?;
        for j in 0..unsafe { se.GetCount() }? {
            let ctl = unsafe { se.GetSession(j) }?;
            let c2: IAudioSessionControl2 = ctl.cast()?;
            if unsafe { c2.IsSystemSoundsSession() } == S_OK || unsafe { ctl.GetState() }.map_or(2, |s| s.0) == 2 {
                continue;
            }
            let pid = unsafe { c2.GetProcessId() }.unwrap_or(0);
            ss.push(S { ep, pid, meter: ctl.cast()?, vol: ctl.cast()?, trace: Vec::new() });
        }
    }
    let (mut polls, mut cost, mut worst) = (0u32, Duration::ZERO, Duration::ZERO);
    let t = Instant::now();
    while t.elapsed().as_secs_f32() < secs {
        let t0 = Instant::now();
        for s in &mut ss {
            s.trace.push(unsafe { s.meter.GetPeakValue() }.unwrap_or(-1.0));
        }
        let d = t0.elapsed();
        cost += d;
        worst = worst.max(d);
        for (_, m, trace) in &mut eps {
            trace.push(unsafe { m.GetPeakValue() }.unwrap_or(-1.0));
        }
        polls += 1;
        std::thread::sleep(Duration::from_millis(10));
    }
    println!(
        "{polls} polls of {} session meters: mean {:.1} us per session, worst poll {:.0} us",
        ss.len(),
        cost.as_secs_f64() * 1e6 / f64::from(polls) / ss.len().max(1) as f64,
        worst.as_secs_f64() * 1e6
    );
    let fmt = |v: &[f32]| v.iter().step_by(10).take(12).map(|p| format!("{p:.5}")).collect::<Vec<_>>().join(" ");
    for (k, (name, _, trace)) in eps.iter().enumerate() {
        println!("endpoint {k} '{name}': {}", fmt(trace));
        for s in ss.iter().filter(|s| s.ep == k) {
            let v = unsafe { s.vol.GetMasterVolume() }.unwrap_or(-1.0);
            let muted = unsafe { s.vol.GetMute() }.is_ok_and(|m| m.as_bool());
            let exe = sidestage_exe(s.pid);
            println!(
                "   pid {:>6} {exe:<24} vol {v:.6}{}: {}",
                s.pid,
                if muted { " MUTED" } else { "" },
                fmt(&s.trace)
            );
        }
    }
    for (a, sa) in ss.iter().enumerate() {
        for sb in &ss[a + 1..] {
            let same = sa.trace.iter().zip(&sb.trace).filter(|(x, y)| **x > 0.0 && x.to_bits() == y.to_bits()).count();
            if sa.ep == sb.ep && sa.pid != sb.pid && same > 0 {
                println!("IDENTICAL nonzero peaks: pid {} and pid {} in {same}/{polls} polls", sa.pid, sb.pid);
            }
        }
    }
    return Ok(());

    fn sidestage_exe(pid: u32) -> String {
        use windows::Win32::System::Threading::{
            OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
        };
        let Ok(h) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
            return "?".into();
        };
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = unsafe {
            QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut len)
        };
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(h) };
        if ok.is_err() {
            return "?".into();
        }
        String::from_utf16_lossy(&buf[..len as usize]).rsplit('\\').next().unwrap_or("?").to_owned()
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("win_meters is a Windows-only diagnostic");
}
