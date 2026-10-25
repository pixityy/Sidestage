//! A quiet, steady test tone for the Windows backend's live test (`win_e2e`).
//!
//! Plays a sine through cpal to "Steam Streaming Speakers" (a virtual output nobody hears) at
//! amplitude 0.01 when that device exists, otherwise to the default output at amplitude 0.001
//! (-60 dBFS, barely audible). `--freq <hz>` (default 997) and `--amp <a>` (at most those
//! levels) set the tone. Prints one line,
//! `TONE pid=<pid> amp=<amp> freq=<hz> rate=<hz> device=<name>`, then plays for `--seconds`
//! (default 300) or until killed. `--delay-ms <n>` waits that long after printing the line
//! before it opens its audio stream (an app that starts playing later). `--start-at <ms>` opens
//! the stream at once but plays silence until that Unix time in milliseconds (a sound that
//! starts in a stream that is already open, for latency measurements).
//!
//! ```text
//! cargo run -p sidestage-platform --example win_tone -- --seconds 30 --freq 440 --amp 0.005
//! ```

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use std::io::Write;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use anyhow::{Context, bail};
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    const VIRTUAL: &str = "Steam Streaming Speakers";

    let mut seconds = 300u64;
    let mut delay_ms = 0u64;
    let mut start_at = 0u128;
    let mut freq = 997.0f32;
    let mut want_amp: Option<f32> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--seconds" => seconds = args.next().context("--seconds needs a value")?.parse()?,
            "--delay-ms" => delay_ms = args.next().context("--delay-ms needs a value")?.parse()?,
            "--start-at" => start_at = args.next().context("--start-at needs a value")?.parse()?,
            "--freq" => freq = args.next().context("--freq needs a value")?.parse()?,
            "--amp" => want_amp = Some(args.next().context("--amp needs a value")?.parse()?),
            other => bail!("unknown argument {other}"),
        }
    }

    let host = cpal::default_host();
    let name_of = |d: &cpal::Device| d.description().map(|x| x.name().to_owned()).unwrap_or_else(|_| d.to_string());
    let virtual_dev = host.output_devices()?.find(|d| name_of(d).contains(VIRTUAL));
    // Inaudible virtual output: -40 dBFS. Real speakers or headphones: -60 dBFS.
    let (device, max_amp) = match virtual_dev {
        Some(d) => (d, 0.01f32),
        None => (host.default_output_device().context("no output device")?, 0.001f32),
    };
    let amp = want_amp.map_or(max_amp, |a| a.clamp(0.0, max_amp));
    assert!(amp <= 0.01, "the test tone must stay quiet");
    let name = name_of(&device);
    let config = device.default_output_config()?;
    let rate = config.sample_rate();
    let channels = config.channels() as usize;
    let format = config.sample_format();
    let config: cpal::StreamConfig = config.into();

    let mut phase = 0f32;
    let step = freq / rate as f32;
    let mut started = start_at == 0;
    let mut next = move || {
        if !started {
            // Test tool: a clock read per sample is fine here.
            let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
            if now < start_at {
                return 0.0;
            }
            started = true;
        }
        let s = (phase * std::f32::consts::TAU).sin() * amp;
        phase = (phase + step).fract();
        s
    };
    println!("TONE pid={} amp={amp} freq={freq} rate={rate} device={name}", std::process::id());
    std::io::stdout().flush()?;
    std::thread::sleep(Duration::from_millis(delay_ms));

    let err = |e: cpal::Error| eprintln!("win_tone stream error: {e}");
    let stream = match format {
        cpal::SampleFormat::F32 => device.build_output_stream(
            config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                for frame in data.chunks_mut(channels) {
                    let s = next();
                    frame.fill(s);
                }
            },
            err,
            None,
        )?,
        cpal::SampleFormat::I16 => device.build_output_stream(
            config,
            move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                for frame in data.chunks_mut(channels) {
                    let s = (next() * i16::MAX as f32) as i16;
                    frame.fill(s);
                }
            },
            err,
            None,
        )?,
        other => bail!("unsupported sample format {other}"),
    };
    stream.play()?;
    std::thread::sleep(Duration::from_secs(seconds));
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("win_tone is a Windows-only test helper");
}
