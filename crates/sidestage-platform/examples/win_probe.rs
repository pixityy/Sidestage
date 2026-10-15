//! Measures another app's audio through the public `Platform` API without changing it
//! (`HideMethod::KeepOriginal`): prints left/right RMS once per second. Used to check what
//! Sidestage itself plays, e.g. that a front-right preset really is louder on the right.
//!
//! ```text
//! cargo run -p sidestage-platform --example win_probe -- sidestage 5 [output-device-id]
//! ```

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use anyhow::Context;
    use sidestage_platform::{HideMethod, Processor, SessionConfig};

    #[derive(Default)]
    struct Acc {
        l: f64,
        r: f64,
        n: u64,
    }
    struct Meter(Arc<Mutex<Acc>>);
    impl Processor for Meter {
        fn process(&mut self, input: &[f32], output: &mut [f32], _sidechain_peak: f32) {
            // Test-only meter: a lock is acceptable here, the probe's own output is silent.
            if let Ok(mut a) = self.0.try_lock() {
                for f in input.as_chunks::<2>().0 {
                    a.l += f64::from(f[0]) * f64::from(f[0]);
                    a.r += f64::from(f[1]) * f64::from(f[1]);
                    a.n += 1;
                }
            }
            output.fill(0.0);
        }
    }

    let mut args = std::env::args().skip(1);
    let needle = args.next().context("usage: win_probe <app> [seconds] [output id]")?.to_lowercase();
    let seconds: u64 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(5);
    let output = args.next();

    let mut platform = sidestage_platform::platform()?;
    let app = platform
        .list_apps()?
        .into_iter()
        .find(|a| a.name.to_lowercase().contains(&needle) || a.key.0.contains(&needle))
        .with_context(|| format!("no app matching '{needle}' has audio"))?;
    println!("probing {} ({})", app.name, app.key.0);

    let acc = Arc::new(Mutex::new(Acc::default()));
    let shared = acc.clone();
    let session = platform.start(
        SessionConfig { app: app.key, output, hide: HideMethod::KeepOriginal, sidechain: false },
        Arc::new(move |_| Box::new(Meter(shared.clone()))),
    )?;
    let db = |sum: f64, n: u64| 10.0 * (sum / n.max(1) as f64).max(1e-20).log10();
    let end = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < end {
        std::thread::sleep(Duration::from_secs(1));
        let mut a = acc.lock().unwrap_or_else(|e| e.into_inner());
        if a.n > 0 {
            println!(
                "L {:7.2} dB   R {:7.2} dB   R-L {:+6.2} dB",
                db(a.l, a.n),
                db(a.r, a.n),
                db(a.r, a.n) - db(a.l, a.n)
            );
        } else {
            println!("(no audio yet: {:?})", session.status().state);
        }
        *a = Acc::default();
    }
    session.stop()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("win_probe only runs on Windows");
}
