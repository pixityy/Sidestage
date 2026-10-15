//! Screenshot mode for checking the UI without a person at the screen:
//! `SIDESTAGE_SCREENSHOT=out.png sidestage` renders the demo for a moment, saves the window as a
//! PNG and quits (also writes the window icon as `out.icon.png`).

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use eframe::egui::{self, ColorImage, Context, UserData, ViewportCommand};

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
