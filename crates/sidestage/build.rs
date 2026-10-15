//! Build script for the `sidestage` binary. Only macOS builds need anything:
//!
//! * The executable gets `packaging/macos/Info.plist` (version filled in) in its
//!   `__TEXT,__info_plist` section. Without `NSAudioCaptureUsageDescription` macOS never shows
//!   the audio-capture prompt and process taps silently deliver zeros, so even an unbundled
//!   `cargo run` binary needs it. `packaging/macos/make-app.sh` puts the same file into
//!   `Sidestage.app/Contents/Info.plist`.
//! * The deployment target is raised to macOS 14.2 (unless set higher): Core Audio process
//!   taps start there, and Rust links their symbols strongly, so the binary should declare that
//!   floor (`LC_BUILD_VERSION minos`) and be refused cleanly by older systems.
//!
//! Every other target is left alone; the `-sectcreate` linker flag would break their links.

use std::env;
use std::error::Error;
use std::fs;
use std::path::PathBuf;

/// First macOS with Core Audio process taps.
const MIN_MACOS: (u32, u32) = (14, 2);
const VERSION_MARKER: &str = "@VERSION@";

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return Ok(());
    }

    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").ok_or("CARGO_MANIFEST_DIR is not set")?);
    let template_path = manifest.join("../../packaging/macos/Info.plist");
    println!("cargo:rerun-if-changed={}", template_path.display());
    let template =
        fs::read_to_string(&template_path).map_err(|e| format!("reading {}: {e}", template_path.display()))?;
    if !template.contains(VERSION_MARKER) {
        return Err(format!("{} must contain {VERSION_MARKER}", template_path.display()).into());
    }
    let version = env::var("CARGO_PKG_VERSION")?;
    let plist = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?).join("Info.plist");
    fs::write(&plist, template.replace(VERSION_MARKER, &version))?;
    let plist = plist.display().to_string();
    if plist.contains(',') {
        // `-Wl,` splits its argument at commas.
        return Err(format!("cannot embed Info.plist from a path containing a comma: {plist}").into());
    }
    println!("cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{plist}");

    // rustc reads MACOSX_DEPLOYMENT_TARGET from its environment for codegen and the link.
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
    let requested = env::var("MACOSX_DEPLOYMENT_TARGET").ok();
    if requested.as_deref().and_then(parse_version).is_none_or(|v| v < MIN_MACOS) {
        if let Some(v) = &requested {
            println!(
                "cargo:warning=MACOSX_DEPLOYMENT_TARGET={v} is not macOS 14.2 or newer, which Sidestage needs; using 14.2"
            );
        }
        println!("cargo:rustc-env=MACOSX_DEPLOYMENT_TARGET={}.{}", MIN_MACOS.0, MIN_MACOS.1);
    }
    Ok(())
}

fn parse_version(s: &str) -> Option<(u32, u32)> {
    let mut parts = s.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().map_or(Some(0), |m| m.parse().ok())?;
    Some((major, minor))
}
