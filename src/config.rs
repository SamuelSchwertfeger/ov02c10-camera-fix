//! Runtime configuration and command-line parsing.

pub const GAIN_MIN: i32 = 16;
pub const GAIN_MAX: i32 = 248;

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    /// Feed the loopback device continuously.
    Loopback,
    /// Hold the loopback device open, run the sensor only while something reads it.
    OnDemand,
    /// Capture one calibrated frame to a PPM file and exit.
    Snapshot(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// V4L2 device node for the IPU6 ISYS capture endpoint.
    pub capture_device: String,
    pub media_device: String,
    /// Native sensor width in pixels, including padding.
    pub sensor_width: usize,
    pub sensor_height: usize,
    /// Media bus format for the CSI2 receiver. This is a hardware negotiation
    /// value, not a software label: SRGGB10_1X10 breaks VIDIOC_STREAMON.
    pub sensor_format: String,
    pub output_width: usize,
    pub output_height: usize,
    pub loopback_device: String,
    pub num_buffers: u32,
    /// Starting analogue_gain (16-248). Auto-exposure moves it from here.
    pub analogue_gain: i32,
    /// digital_gain (1024-16383), never touched by auto-exposure.
    pub digital_gain: i32,
    pub auto_exposure: bool,
    /// Mean raw brightness (0-255) auto-exposure aims for.
    pub ae_target: f64,
    /// On-demand mode: seconds without a reader before the sensor stops.
    pub idle_secs: u64,
    pub verbose: bool,
    pub mode: Mode,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            capture_device: "/dev/video32".into(),
            media_device: "/dev/media0".into(),
            sensor_width: 1928,
            sensor_height: 1092,
            sensor_format: "SGRBG10_1X10".into(),
            output_width: 1920,
            output_height: 1080,
            loopback_device: "/dev/video48".into(),
            num_buffers: 4,
            analogue_gain: 150,
            digital_gain: 4096,
            auto_exposure: true,
            ae_target: 128.0,
            idle_secs: 5,
            verbose: false,
            mode: Mode::Loopback,
        }
    }
}

pub const USAGE: &str = "\
ov02c10-camera: OV02C10 / Intel IPU6 webcam bridge (raw capture -> v4l2loopback)

USAGE:
    ov02c10-camera <MODE> [OPTIONS]

MODES (pick one):
    --loopback             Feed the loopback device continuously
    --on-demand            Keep the loopback device visible to apps, run the
                           sensor only while something is reading from it
    --snapshot <FILE>      Save one calibrated frame as a PPM image and exit

OPTIONS:
    --device <PATH>            Capture device            [/dev/video32]
    --media-device <PATH>      Media controller device   [/dev/media0]
    --loopback-device <PATH>   v4l2loopback device       [/dev/video48]
    --width <PX>               Output width (even)       [1920]
    --height <PX>              Output height             [1080]
    --analogue-gain <16-248>   Starting analogue gain    [150]
    --digital-gain <1024-16383>  Digital gain            [4096]
    --no-auto-exposure         Keep analogue gain fixed
    --ae-target <1-255>        Auto-exposure brightness target [128]
    --idle-secs <N>            On-demand: stop sensor after N s unused [5]
    -v, --verbose              Debug logging
    -h, --help                 Show this help
    -V, --version              Show version
";

/// Parse arguments (without argv[0]). `Err` carries the message to print.
pub fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Result<Config, String> {
    fn value<T: std::str::FromStr>(
        flag: &str,
        it: &mut impl Iterator<Item = String>,
    ) -> Result<T, String> {
        let v = it.next().ok_or_else(|| format!("{flag} needs a value"))?;
        v.parse()
            .map_err(|_| format!("invalid value for {flag}: {v}"))
    }

    let mut cfg = Config::default();
    let mut mode = None;
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--loopback" => mode = Some(Mode::Loopback),
            "--on-demand" => mode = Some(Mode::OnDemand),
            "--snapshot" => mode = Some(Mode::Snapshot(value(&arg, &mut it)?)),
            "--device" => cfg.capture_device = value(&arg, &mut it)?,
            "--media-device" => cfg.media_device = value(&arg, &mut it)?,
            "--loopback-device" => cfg.loopback_device = value(&arg, &mut it)?,
            "--width" => cfg.output_width = value(&arg, &mut it)?,
            "--height" => cfg.output_height = value(&arg, &mut it)?,
            "--analogue-gain" => cfg.analogue_gain = value(&arg, &mut it)?,
            "--digital-gain" => cfg.digital_gain = value(&arg, &mut it)?,
            "--no-auto-exposure" => cfg.auto_exposure = false,
            "--ae-target" => cfg.ae_target = value(&arg, &mut it)?,
            "--idle-secs" => cfg.idle_secs = value(&arg, &mut it)?,
            "-v" | "--verbose" => cfg.verbose = true,
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    cfg.mode = mode.ok_or("pick a mode: --loopback, --on-demand or --snapshot <FILE>")?;

    if cfg.output_width == 0 || cfg.output_width % 2 != 0 || cfg.output_width > 8192 {
        return Err("--width must be an even number between 2 and 8192".into());
    }
    if cfg.output_height == 0 || cfg.output_height > 8192 {
        return Err("--height must be between 1 and 8192".into());
    }
    if !(GAIN_MIN..=GAIN_MAX).contains(&cfg.analogue_gain) {
        return Err(format!("--analogue-gain must be {GAIN_MIN}-{GAIN_MAX}"));
    }
    if !(1024..=16383).contains(&cfg.digital_gain) {
        return Err("--digital-gain must be 1024-16383".into());
    }
    if !(1.0..=255.0).contains(&cfg.ae_target) {
        return Err("--ae-target must be 1-255".into());
    }
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Config, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn defaults_match_known_working_hardware_values() {
        let cfg = Config::default();
        assert_eq!(cfg.sensor_width, 1928);
        assert_eq!(cfg.sensor_height, 1092);
        assert_eq!(cfg.sensor_format, "SGRBG10_1X10");
        assert_eq!(cfg.analogue_gain, 150);
        assert_eq!(cfg.digital_gain, 4096);
        assert_eq!(cfg.output_width, 1920);
        assert_eq!(cfg.output_height, 1080);
        assert!(cfg.auto_exposure);
    }

    #[test]
    fn each_mode_flag_selects_its_mode() {
        assert_eq!(parse(&["--loopback"]).unwrap().mode, Mode::Loopback);
        assert_eq!(parse(&["--on-demand"]).unwrap().mode, Mode::OnDemand);
        assert_eq!(
            parse(&["--snapshot", "a.ppm"]).unwrap().mode,
            Mode::Snapshot("a.ppm".into())
        );
    }

    #[test]
    fn missing_mode_is_an_error() {
        assert!(parse(&[]).unwrap_err().contains("pick a mode"));
        assert!(parse(&["-v", "--width", "640"]).is_err());
    }

    #[test]
    fn options_override_only_their_fields() {
        let cfg = parse(&[
            "--loopback",
            "--analogue-gain",
            "200",
            "--width",
            "640",
            "--height",
            "480",
            "--no-auto-exposure",
            "-v",
        ])
        .unwrap();
        assert_eq!(cfg.analogue_gain, 200);
        assert_eq!(cfg.output_width, 640);
        assert_eq!(cfg.output_height, 480);
        assert!(!cfg.auto_exposure);
        assert!(cfg.verbose);
        assert_eq!(cfg.digital_gain, 4096);
    }

    #[test]
    fn bad_width_is_rejected() {
        assert!(parse(&["--loopback", "--width", "641"]).is_err());
        assert!(parse(&["--loopback", "--width", "0"]).is_err());
        assert!(parse(&["--loopback", "--width", "abc"]).is_err());
        assert!(parse(&["--loopback", "--width", "8194"]).is_err());
    }

    #[test]
    fn out_of_range_values_are_rejected() {
        assert!(parse(&["--loopback", "--analogue-gain", "15"]).is_err());
        assert!(parse(&["--loopback", "--analogue-gain", "249"]).is_err());
        assert!(parse(&["--loopback", "--analogue-gain", "16"]).is_ok());
        assert!(parse(&["--loopback", "--analogue-gain", "248"]).is_ok());
        assert!(parse(&["--loopback", "--digital-gain", "1023"]).is_err());
        assert!(parse(&["--loopback", "--digital-gain", "16384"]).is_err());
        assert!(parse(&["--loopback", "--ae-target", "0"]).is_err());
        assert!(parse(&["--loopback", "--height", "0"]).is_err());
    }

    #[test]
    fn unknown_flag_is_rejected() {
        let err = parse(&["--loopback", "--bogus"]).unwrap_err();
        assert!(err.contains("unknown argument"));
    }

    #[test]
    fn missing_value_is_rejected() {
        assert!(parse(&["--loopback", "--width"])
            .unwrap_err()
            .contains("needs a value"));
        assert!(parse(&["--snapshot"]).is_err());
    }
}
