//! Read-only diagnostic: prints every audio session on every active output (what the Windows
//! Volume mixer shows), plus the backend's app list, outputs and hide methods. Changes nothing.
//!
//! ```text
//! cargo run -p sidestage-platform --example win_sessions
//! ```

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use windows::Win32::Foundation::S_OK;
    use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
    use windows::Win32::Media::Audio::{
        DEVICE_STATE_ACTIVE, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator, ISimpleAudioVolume,
        MMDeviceEnumerator, eRender,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    };
    use windows::core::Interface;

    if let Some(code) = sidestage_platform::run_helper_if_requested() {
        std::process::exit(code);
    }
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
    let en: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }?;
    let devs = unsafe { en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }?;
    for i in 0..unsafe { devs.GetCount() }? {
        let dev = unsafe { devs.Item(i) }?;
        let id = unsafe { dev.GetId() }?;
        println!("endpoint {}", unsafe { id.to_string() }.unwrap_or_default());
        unsafe { CoTaskMemFree(Some(id.0 as *const _)) };
        let mgr: IAudioSessionManager2 = unsafe { dev.Activate(CLSCTX_ALL, None) }?;
        let se = unsafe { mgr.GetSessionEnumerator() }?;
        for j in 0..unsafe { se.GetCount() }? {
            let ctl = unsafe { se.GetSession(j) }?;
            let c2: IAudioSessionControl2 = ctl.cast()?;
            let vol: ISimpleAudioVolume = ctl.cast()?;
            let meter: IAudioMeterInformation = ctl.cast()?;
            unsafe {
                println!(
                    "  pid {:>6} state {} system {} volume {:.6} muted {} peak {:.6}",
                    c2.GetProcessId().unwrap_or(0),
                    ctl.GetState().map_or(-1, |s| s.0),
                    c2.IsSystemSoundsSession() == S_OK,
                    vol.GetMasterVolume().unwrap_or(-1.0),
                    vol.GetMute().map(|b| b.as_bool()).unwrap_or(false),
                    meter.GetPeakValue().unwrap_or(-1.0),
                );
            }
        }
    }

    let mut p = sidestage_platform::platform()?;
    println!("\n{} / {:?}", p.name(), p.support());
    for a in p.list_apps()? {
        println!(
            "app {:<24} {:<20} playing {} peak {:?} pids {:?} key {}",
            a.name, a.detail, a.playing, a.peak, a.pids, a.key.0
        );
    }
    for o in p.list_outputs()? {
        println!("output {}{}", if o.is_default { "* " } else { "  " }, o.name);
    }
    for (m, label) in p.hide_methods() {
        println!("hide {m:?}: {label}");
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("win_sessions is Windows-only");
}
