//! Screenshot mode for checking the UI without a person at the screen:
//! `SIDESTAGE_SCREENSHOT=out.png sidestage` renders the demo for a moment, saves the window as a
//! PNG and quits (also writes the window icon as `out.icon.png`). The "real" scene shows the OS
//! backend's apps and outputs through [`Observer`], which never captures anything.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use eframe::egui::{self, ColorImage, Context, UserData, ViewportCommand};
use sidestage_platform::{AudioApp, HideMethod, OutputDevice, Platform, Session, SessionConfig, Support};

/// The OS backend as an unattended screenshot run sees it: apps, outputs and hide methods, but
/// no session and no undoing of an earlier run's leftovers. Whatever the window is clicked into
/// while the screenshot settles, no app is hidden and nothing the user's own Sidestage changed
/// is touched.
pub struct Observer(pub Box<dyn Platform>);

impl Platform for Observer {
    fn name(&self) -> &'static str {
        self.0.name()
    }

    fn support(&self) -> Support {
        self.0.support()
    }

    fn list_apps(&mut self) -> anyhow::Result<Vec<AudioApp>> {
        self.0.list_apps()
    }

    fn list_outputs(&mut self) -> anyhow::Result<Vec<OutputDevice>> {
        self.0.list_outputs()
    }

    fn hide_methods(&mut self) -> Vec<(HideMethod, String)> {
        self.0.hide_methods()
    }

    fn start(&mut self, _config: SessionConfig) -> anyhow::Result<Box<dyn Session>> {
        anyhow::bail!("screenshot runs never capture")
    }

    fn recover(&mut self) -> anyhow::Result<Vec<String>> {
        Ok(Vec::new())
    }
}

/// Long enough for the demo engine to start its session and the meters to move.
const SETTLE: Duration = Duration::from_millis(1800);
/// Give up if the renderer never answers.
const TIMEOUT: Duration = Duration::from_secs(20);

pub struct Screenshot {
    path: PathBuf,
    started: Instant,
    frames: u32,
    requested: bool,
    done: bool,
}

impl Screenshot {
    pub fn from_env() -> Option<Self> {
        let path = std::env::var_os("SIDESTAGE_SCREENSHOT").filter(|p| !p.is_empty())?;
        Some(Self { path: PathBuf::from(path), started: Instant::now(), frames: 0, requested: false, done: false })
    }

    /// Call once per frame.
    pub fn tick(&mut self, ctx: &Context) {
        if self.done {
            return;
        }
        self.frames += 1;
        ctx.request_repaint();
        let image = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = image {
            match save(&self.path, &image) {
                Ok(()) => log::info!("screenshot saved to {}", self.path.display()),
                Err(e) => log::error!("screenshot failed: {e:#}"),
            }
            let icon = crate::ui::icon_image();
            if let Err(e) = save(&self.path.with_extension("icon.png"), &icon) {
                log::error!("icon export failed: {e:#}");
            }
            self.done = true;
            crate::ui::close(ctx);
            return;
        }
        let elapsed = self.started.elapsed();
        if !self.requested && self.frames > 10 && elapsed >= SETTLE {
            ctx.send_viewport_cmd(ViewportCommand::Screenshot(UserData::default()));
            self.requested = true;
        }
        if elapsed > TIMEOUT {
            log::error!("no screenshot arrived within {TIMEOUT:?}; giving up");
            self.done = true;
            crate::ui::close(ctx);
        }
    }
}

fn save(path: &Path, image: &ColorImage) -> anyhow::Result<()> {
    let [w, h] = image.size;
    let mut bytes = Vec::with_capacity(w * h * 4);
    for c in &image.pixels {
        bytes.extend_from_slice(&c.to_srgba_unmultiplied());
    }
    let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), u32::try_from(w)?, u32::try_from(h)?);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&bytes)?;
    writer.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use sidestage_platform::{AppKey, SourceConfig};

    use super::*;
    use crate::demo::{DemoPlatform, Scenario};

    #[test]
    fn observer_lists_but_never_captures_or_restores() {
        let starts = Arc::new(AtomicUsize::new(0));
        let demo = DemoPlatform::new(Scenario { warnings: true, ..Scenario::default() })
            .with_start_counter(Arc::clone(&starts));
        let mut observer = Observer(Box::new(demo));
        assert!(!observer.list_apps().unwrap().is_empty(), "the apps are listed");
        assert!(!observer.list_outputs().unwrap().is_empty());
        assert!(observer.recover().unwrap().is_empty(), "an earlier run's leftovers are not touched");
        let config = SessionConfig {
            output: None,
            sidechain: true,
            sidechain_ignore: Vec::new(),
            sources: vec![SourceConfig {
                id: 1,
                app: AppKey("demo:spotify".into()),
                hide: HideMethod::Auto,
                processor: crate::processor::master_factory(),
            }],
            master: crate::processor::master_factory(),
        };
        assert!(observer.start(config).is_err());
        assert_eq!(starts.load(Ordering::Relaxed), 0, "the backend was never asked to start");
    }
}
