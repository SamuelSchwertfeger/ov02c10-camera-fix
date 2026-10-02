//! The capture loop and the three run modes. Linux only.

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use std::{fs, thread};

use crate::config::{Config, Mode};
use crate::exposure::AutoExposure;
use crate::image::{self, Debayer};
use crate::media::Sensor;
use crate::v4l2::{Capture, Loopback};

/// Frames between auto-exposure measurements once the stream is running.
const AE_INTERVAL: u64 = 10;
/// Frames to wait after a gain change before trusting brightness again.
const SETTLE_FRAMES: u32 = 4;
/// Startup calibration gives up converging after this many frames.
const CALIBRATION_LIMIT: u64 = 60;
/// Consecutive 2 s frame timeouts before the stream counts as dead.
const MAX_TIMEOUTS: u32 = 5;
/// Never killed for holding the capture node: WirePlumber keeps a
/// monitoring handle on camera devices, and killing it takes the whole
/// desktop's audio routing down with it.
const CRITICAL_PROCESSES: [&str; 3] = ["pipewire", "wireplumber", "pipewire-media-session"];

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

fn stopping() -> bool {
    STOP.load(Ordering::Relaxed)
}

/// A running sensor stream that turns raw frames into calibrated Bayer.
struct Stream {
    cam: Capture,
    sensor: Sensor,
    ae: Option<AutoExposure>,
    debayer: Debayer,
    bayer: Vec<u8>,
    /// Until exposure has converged and white balance is measured, frames
    /// are held back.
    calibrating: bool,
    settle: u32,
    frame_no: u64,
    first_frame: bool,
    timeouts: u32,
}

impl Stream {
    fn start(cfg: &Config, analogue_gain: i32) -> io::Result<Self> {
        free_device(&cfg.capture_device);
        let sensor = Sensor::find(&cfg.media_device)?;
        let cam = Capture::open(cfg, &sensor, analogue_gain)?;
        let (w, h) = (cfg.sensor_width, cfg.sensor_height);
        Ok(Self {
            cam,
            sensor,
            ae: cfg
                .auto_exposure
                .then(|| AutoExposure::new(analogue_gain, cfg.ae_target)),
            debayer: Debayer::new(w, h, cfg.output_width, cfg.output_height),
            bayer: vec![0; w * h],
            calibrating: true,
            settle: 0,
            frame_no: 0,
            first_frame: true,
            timeouts: 0,
        })
    }

    /// Current analogue gain, so a restarted stream can pick up where this
    /// one left off.
    fn gain(&self, cfg: &Config) -> i32 {
        self.ae.as_ref().map_or(cfg.analogue_gain, |ae| ae.gain)
    }

    /// Process one sensor frame. True when `self.bayer` holds a new calibrated frame.
    fn next(&mut self, cfg: &Config) -> io::Result<bool> {
        let (w, h, stride) = (cfg.sensor_width, cfg.sensor_height, self.cam.stride);
        let bayer = &mut self.bayer;
        match self
            .cam
            .frame(|raw| image::unpack_sgrbg10(raw, w, h, stride, bayer))?
        {
            Some(true) => self.timeouts = 0,
            Some(false) => {
                warn!("short frame from sensor, skipped");
                return Ok(false);
            }
            None => {
                if stopping() {
                    return Ok(false);
                }
                self.timeouts += 1;
                warn!(
                    "timeout waiting for frame ({}/{MAX_TIMEOUTS})",
                    self.timeouts
                );
                if self.timeouts >= MAX_TIMEOUTS {
                    return Err(io::Error::other("sensor stopped delivering frames"));
                }
                return Ok(false);
            }
        }
        // The first frame after stream start is an overexposed warm-up frame.
        if std::mem::take(&mut self.first_frame) {
            return Ok(false);
        }
        self.frame_no += 1;
        let hold = self.calibrating && self.frame_no < CALIBRATION_LIMIT;

        if self.settle > 0 {
            self.settle -= 1;
            if hold {
                return Ok(false);
            }
        } else if let Some(ae) = &mut self.ae {
            if self.calibrating || self.frame_no % AE_INTERVAL == 0 {
                let (mean, clipped) = image::frame_stats(&self.bayer);
                if let Some(gain) = ae.step(mean, clipped) {
                    info!(
                        "Auto-exposure: brightness {mean:.0}/255, {:.0}% clipped -> analogue_gain {gain}",
                        clipped * 100.0
                    );
                    self.sensor.set_controls(&format!("analogue_gain={gain}"))?;
                    self.settle = SETTLE_FRAMES;
                    if hold {
                        return Ok(false);
                    }
                }
            }
        }

        if self.calibrating {
            self.calibrating = false;
            self.debayer.wb = image::white_balance(&self.bayer, w, h);
            let (red, blue) = self.debayer.wb;
            info!("White balance: red x{red}/64, blue x{blue}/64");
        }
        Ok(true)
    }
}

/// Kill whatever else holds the capture node open (a stale instance, a
/// stray `cam`), except the desktop's media services.
fn free_device(device: &str) {
    let target = fs::canonicalize(device).unwrap_or_else(|_| device.into());
    let me = std::process::id();
    let mut killed = false;
    for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if pid == me || !holds(&entry.path().join("fd"), &target) {
            continue;
        }
        let comm = fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
        let comm = comm.trim();
        if CRITICAL_PROCESSES.contains(&comm) {
            info!("{comm} (pid {pid}) holds {device}: leaving it alone");
        } else {
            warn!("{device} is held by {comm} (pid {pid}): killing it");
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            killed = true;
        }
    }
    if killed {
        thread::sleep(Duration::from_secs(1));
    }
}

/// Whether any fd in a `/proc/<pid>/fd` directory points at `target`.
fn holds(fd_dir: &Path, target: &Path) -> bool {
    fs::read_dir(fd_dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|fd| fs::read_link(fd.path()).is_ok_and(|link| link == target))
}

fn snapshot(cfg: &Config, path: &str) -> io::Result<()> {
    let mut stream = Stream::start(cfg, cfg.analogue_gain)?;
    // Take a frame a little after calibration so exposure has settled.
    let mut good = 0;
    while good < 10 {
        if stopping() {
            return Ok(());
        }
        good += stream.next(cfg)? as u32;
    }
    let (w, h) = (cfg.output_width, cfg.output_height);
    let mut ppm = image::ppm_header(w, h);
    let head = ppm.len();
    ppm.resize(head + w * h * 3, 0);
    stream.debayer.run_rgb(&stream.bayer, &mut ppm[head..]);
    fs::write(path, ppm)?;
    info!("Wrote {path}");
    Ok(())
}

/// Feed the loopback device. With `on_demand`, the sensor only runs while
/// another application is actually streaming from it.
///
/// Either way this process holds the loopback device open from the start
/// and writes a black frame: with `exclusive_caps=1` the device only shows
/// up as a camera while a producer is attached, so a browser could never
/// open (and thereby wake) a camera whose producer starts on demand.
fn loopback(cfg: &Config, on_demand: bool) -> io::Result<()> {
    let (w, h) = (cfg.output_width, cfg.output_height);
    let lb = Loopback::open(cfg)?;
    let mut yuyv = vec![0u8; w * h * 2];
    image::fill_black_yuyv(&mut yuyv);
    lb.write(&yuyv)?;
    info!("{} is ready ({w}x{h} YUYV)", cfg.loopback_device);

    let on_demand = on_demand && {
        let supported = lb.subscribe();
        if !supported {
            warn!("this v4l2loopback has no reader events; running the sensor continuously");
        }
        supported
    };
    let idle = Duration::from_secs(cfg.idle_secs);
    let mut gain = cfg.analogue_gain;
    let mut stream: Option<Stream> = None;
    let mut wanted = !on_demand;
    let mut idle_since: Option<Instant> = None;

    while !stopping() {
        if on_demand {
            // Sleep on the event while idle, only peek while streaming.
            let timeout_ms = if stream.is_some() { 0 } else { 1000 };
            if let Some(reading) = lb.reader_change(timeout_ms)? {
                debug!("reader streaming: {reading}");
                wanted = reading;
                idle_since = (!reading).then(Instant::now);
            }
        }
        let expired = !wanted && idle_since.is_none_or(|t| t.elapsed() >= idle);
        match stream.as_mut() {
            Some(s) if !expired => {
                if s.next(cfg)? {
                    s.debayer.run_yuyv(&s.bayer, &mut yuyv);
                    lb.write(&yuyv)?;
                }
            }
            Some(s) => {
                gain = s.gain(cfg);
                stream = None;
                image::fill_black_yuyv(&mut yuyv);
                lb.write(&yuyv)?;
                info!("No readers left, sensor stopped");
            }
            None if wanted => {
                info!("Starting sensor");
                stream = Some(Stream::start(cfg, gain)?);
            }
            // Idle: keep a frame flowing so a reader that just attached is
            // not left waiting while the sensor starts.
            None => {
                image::fill_black_yuyv(&mut yuyv);
                lb.write(&yuyv)?;
            }
        }
    }
    info!("Shutting down");
    Ok(())
}

pub fn run(cfg: &Config) -> io::Result<()> {
    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
    match &cfg.mode {
        Mode::Snapshot(path) => snapshot(cfg, path),
        Mode::Loopback => loopback(cfg, false),
        Mode::OnDemand => loopback(cfg, true),
    }
}
