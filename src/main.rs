//! OV02C10 / Intel IPU6 webcam bridge: raw V4L2 capture, software debayer,
//! output to a v4l2loopback device that browsers and video apps can open.
//!
//! Rust port of Seth Barrett's Python implementation; see docs/DEBUGGING.md
//! for why the raw-capture route is the one that works on this hardware.

// Only the hardware-free logic is exercised off Linux (tests, cargo check).
#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_macros))]

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};

pub static VERBOSE: AtomicBool = AtomicBool::new(false);

macro_rules! info {
    ($($arg:tt)*) => { eprintln!($($arg)*) };
}
macro_rules! warn {
    ($($arg:tt)*) => { eprintln!("warning: {}", format_args!($($arg)*)) };
}
macro_rules! debug {
    ($($arg:tt)*) => {
        if $crate::VERBOSE.load(std::sync::atomic::Ordering::Relaxed) {
            eprintln!("debug: {}", format_args!($($arg)*));
        }
    };
}

mod config;
mod exposure;
mod image;
mod media;
#[cfg(target_os = "linux")]
mod run;
mod v4l2;

#[cfg(target_os = "linux")]
fn run(cfg: &config::Config) -> ExitCode {
    info!(
        "ov02c10-camera {}: sensor {}x{} -> output {}x{}",
        env!("CARGO_PKG_VERSION"),
        cfg.sensor_width,
        cfg.sensor_height,
        cfg.output_width,
        cfg.output_height
    );
    match run::run(cfg) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn run(_: &config::Config) -> ExitCode {
    eprintln!("error: this tool drives Linux V4L2 devices and only runs on Linux");
    ExitCode::FAILURE
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{}", config::USAGE);
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("ov02c10-camera {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let cfg = match config::parse_args(args) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("error: {e}\n\n{}", config::USAGE);
            return ExitCode::from(2);
        }
    };
    VERBOSE.store(cfg.verbose, Ordering::Relaxed);
    run(&cfg)
}
