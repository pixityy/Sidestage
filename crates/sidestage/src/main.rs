//! Sidestage: put one app's audio (usually your music) behind you in 3D while your game audio
//! stays exactly as it is.
//!
//! `sidestage` opens the window; `sidestage --help` lists the command-line tools.

// Release builds on Windows are GUI-subsystem executables so no console window flashes up when
// the app is started from Explorer. CLI subcommands attach to the parent console instead
// (see `console::attach_parent`).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod cli;
mod config;
mod console;
mod demo;
mod engine;
mod instance;
mod logging;
mod processor;
mod screenshot;
mod signal;
mod text;
mod ui;

use std::process::ExitCode;

fn main() -> ExitCode {
    // The crash guardian is this same executable; it must not open a window or parse our CLI.
    if let Some(code) = sidestage_platform::run_helper_if_requested() {
        return exit_code(code);
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match cli::parse(&args) {
        Ok(command) => command,
        Err(message) => {
            console::attach_parent();
            console::tell!("sidestage: {message}");
            console::tell!("Run 'sidestage --help' for usage.");
            return ExitCode::from(2);
        }
    };

    match command {
        cli::Command::Gui { demo } => {
            let mut launch = ui::Launch::from_env(demo);
            let (lock, lock_problem) = ui::claim_instance(&mut launch);
            // Only the instance that may capture writes the log file: a second, blocked window, a
            // demo or a screenshot run would otherwise rotate the running instance's log away.
            let file_log = !(launch.blocked || launch.demo || launch.screenshot.is_some());
            logging::init(if file_log { logging::Mode::Gui } else { logging::Mode::Cli });
            if let Some(problem) = lock_problem {
                log::warn!("{problem}");
            }
            // Ctrl+C / SIGTERM close the window normally instead of killing the process mid-session.
            if let Err(e) = signal::install() {
                log::debug!("no termination handler: {e:#}");
            }
            let result = ui::run(launch);
            // The engine has stopped and restored everything by now.
            signal::cleanup_done();
            drop(lock);
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    log::error!("{e:#}");
                    // Without a console nobody would see why nothing opened (screenshot runs are
                    // unattended and must not block on a dialog).
                    if std::env::var_os("SIDESTAGE_SCREENSHOT").is_none() {
                        let log =
                            logging::written_log().map(|p| format!("\n\nLog: {}", p.display())).unwrap_or_default();
                        console::error_box(&format!("Sidestage could not start.\n\n{e:#}{log}"));
                    }
                    ExitCode::FAILURE
                }
            }
        }
        command => {
            console::attach_parent();
            logging::init(logging::Mode::Cli);
            exit_code(cli::execute(command))
        }
    }
}

fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
