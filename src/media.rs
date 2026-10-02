//! media-ctl / v4l2-ctl wiring: sensor discovery, gain control, link and
//! format setup. Shells out to the v4l-utils tools, the path known to work
//! on this hardware.

use std::io;
use std::process::Command;

use crate::config::Config;

/// Find the sensor's media entity name (e.g. `ov02c10 5-0036`) in
/// `media-ctl -p` output.
///
/// The I2C bus number in the name is not stable across boots (ACPI
/// enumerates I2C devices in a different order depending on boot state), so
/// it has to be looked up live rather than hardcoded.
pub fn parse_sensor_entity(topology: &str) -> Option<&str> {
    const PREFIX: &str = "ov02c10 ";
    let mut from = 0;
    while let Some(pos) = topology[from..].find(PREFIX) {
        let start = from + pos;
        let rest = &topology.as_bytes()[start + PREFIX.len()..];
        let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
        let addr = rest.get(digits + 1..digits + 5);
        let is_hex = |b: &u8| b.is_ascii_digit() || (b'a'..=b'f').contains(b);
        if digits > 0
            && rest.get(digits) == Some(&b'-')
            && addr.is_some_and(|a| a.iter().all(is_hex))
        {
            return Some(&topology[start..start + PREFIX.len() + digits + 5]);
        }
        from = start + PREFIX.len();
    }
    None
}

/// The media-ctl invocations that route the sensor through the IVSC CSI
/// bridge and the IPU6 CSI2 receiver into the ISYS capture node, as
/// (description, arguments) pairs.
///
/// The sensor's own pad format is set too: otherwise it keeps whatever the
/// last process (e.g. `cam`) left, which can mismatch what the CSI2
/// receiver is told to expect and cause "Frame sync error" on STREAMON.
pub fn pipeline_steps(cfg: &Config, sensor: &str) -> Vec<(&'static str, Vec<String>)> {
    let fmt = format!(
        "{}/{}x{}",
        cfg.sensor_format, cfg.sensor_width, cfg.sensor_height
    );
    let step =
        |flag: &str, arg: String| vec!["-d".into(), cfg.media_device.clone(), flag.into(), arg];
    let link = |arg: String| step("--links", arg);
    let pad = |entity: &str, n: u8| step("--set-v4l2", format!("\"{entity}\":{n}[fmt:{fmt}]"));
    vec![
        (
            "sensor->IVSC link",
            link(format!("\"{sensor}\":0->\"Intel IVSC CSI\":0[1]")),
        ),
        (
            "CSI2-4->Capture32 link",
            link("\"Intel IPU6 CSI2 4\":1->\"Intel IPU6 ISYS Capture 32\":0[1]".into()),
        ),
        ("sensor fmt", pad(sensor, 0)),
        ("IVSC sink fmt", pad("Intel IVSC CSI", 0)),
        ("IVSC source fmt", pad("Intel IVSC CSI", 1)),
        ("CSI2-4 sink fmt", pad("Intel IPU6 CSI2 4", 0)),
        ("CSI2-4 source fmt", pad("Intel IPU6 CSI2 4", 1)),
    ]
}

/// Run a tool, returning (succeeded, stdout).
fn run(program: &str, args: &[String]) -> io::Result<(bool, String)> {
    debug!("run: {program} {}", args.join(" "));
    let out = Command::new(program).args(args).output().map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("cannot run {program} (is v4l-utils installed?): {e}"),
        )
    })?;
    if !out.status.success() {
        debug!(
            "{program} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    ))
}

/// The sensor as currently enumerated: media entity name plus the
/// `/dev/v4l-subdevN` node its controls live on.
pub struct Sensor {
    pub entity: String,
    pub subdev: String,
}

impl Sensor {
    pub fn find(media_device: &str) -> io::Result<Self> {
        let dev = |args: &[&str]| -> Vec<String> {
            ["-d", media_device]
                .iter()
                .chain(args)
                .map(|s| s.to_string())
                .collect()
        };
        let (ok, topology) = run("media-ctl", &dev(&["-p"]))?;
        let entity = parse_sensor_entity(&topology)
            .filter(|_| ok)
            .ok_or_else(|| {
                io::Error::other(format!(
                    "ov02c10 sensor entity not found in {media_device} topology"
                ))
            })?;
        let (_, subdev) = run("media-ctl", &dev(&["-e", entity]))?;
        let subdev = subdev.trim();
        if subdev.is_empty() {
            return Err(io::Error::other(format!(
                "no subdevice node for '{entity}'"
            )));
        }
        Ok(Self {
            entity: entity.to_string(),
            subdev: subdev.to_string(),
        })
    }

    /// Set sensor controls, e.g. `analogue_gain=150,digital_gain=4096`.
    pub fn set_controls(&self, controls: &str) -> io::Result<()> {
        let (ok, _) = run(
            "v4l2-ctl",
            &[
                "-d".into(),
                self.subdev.clone(),
                "-c".into(),
                controls.into(),
            ],
        )?;
        if ok {
            debug!("set {controls} on {}", self.subdev);
        } else {
            warn!("could not set {controls} on {}", self.subdev);
        }
        Ok(())
    }

    /// Configure links and pad formats. Must happen before buffers are
    /// requested on the capture node: reconfiguring afterwards made
    /// VIDIOC_STREAMON fail on every run after the first.
    pub fn setup_pipeline(&self, cfg: &Config) -> io::Result<()> {
        info!("Configuring media pipeline on {}", cfg.media_device);
        for (desc, args) in pipeline_steps(cfg, &self.entity) {
            let (ok, _) = run("media-ctl", &args)?;
            if ok {
                debug!("  ok: {desc}");
            } else {
                warn!("media-ctl step failed: {desc} (run with -v for details)");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn topology(entity: &str) -> String {
        format!(
            "- entity 368: {entity} (1 pad, 1 link, 0 routes)\n            type V4L2 subdev subtype Sensor flags 0\n            device node name /dev/v4l-subdev3\n"
        )
    }

    #[test]
    fn finds_sensor_with_varying_bus_numbers() {
        for entity in ["ov02c10 5-0036", "ov02c10 14-0036", "ov02c10 0-0010"] {
            assert_eq!(parse_sensor_entity(&topology(entity)), Some(entity));
        }
    }

    #[test]
    fn sensor_not_found() {
        assert_eq!(
            parse_sensor_entity("- entity 1: some other camera (1 pad, 1 link, 0 routes)\n"),
            None
        );
        assert_eq!(parse_sensor_entity(""), None);
        assert_eq!(parse_sensor_entity("ov02c10 x-0036"), None);
        assert_eq!(parse_sensor_entity("ov02c10 5-00"), None);
    }

    #[test]
    fn skips_malformed_match_and_finds_later_one() {
        let t = format!("ov02c10 bad\n{}", topology("ov02c10 5-0036"));
        assert_eq!(parse_sensor_entity(&t), Some("ov02c10 5-0036"));
    }

    #[test]
    fn pipeline_steps_count_order_and_args() {
        let cfg = Config::default();
        let steps = pipeline_steps(&cfg, "ov02c10 5-0036");
        let descs: Vec<_> = steps.iter().map(|s| s.0).collect();
        assert_eq!(
            descs,
            [
                "sensor->IVSC link",
                "CSI2-4->Capture32 link",
                "sensor fmt",
                "IVSC sink fmt",
                "IVSC source fmt",
                "CSI2-4 sink fmt",
                "CSI2-4 source fmt",
            ]
        );
        for (_, args) in &steps {
            assert_eq!(args.len(), 4);
            assert_eq!(args[0], "-d");
            assert_eq!(args[1], "/dev/media0");
        }
        assert_eq!(steps[0].1[2], "--links");
        assert_eq!(
            steps[0].1[3],
            "\"ov02c10 5-0036\":0->\"Intel IVSC CSI\":0[1]"
        );
        assert_eq!(
            steps[1].1[3],
            "\"Intel IPU6 CSI2 4\":1->\"Intel IPU6 ISYS Capture 32\":0[1]"
        );
        assert_eq!(steps[2].1[2], "--set-v4l2");
        assert_eq!(
            steps[2].1[3],
            "\"ov02c10 5-0036\":0[fmt:SGRBG10_1X10/1928x1092]"
        );
        assert_eq!(
            steps[4].1[3],
            "\"Intel IVSC CSI\":1[fmt:SGRBG10_1X10/1928x1092]"
        );
    }

    #[test]
    fn pipeline_steps_use_configured_device_and_geometry() {
        let cfg = Config {
            media_device: "/dev/media3".into(),
            sensor_width: 640,
            sensor_height: 480,
            ..Config::default()
        };
        let steps = pipeline_steps(&cfg, "ov02c10 14-0036");
        assert!(steps.iter().all(|(_, a)| a[1] == "/dev/media3"));
        assert!(steps[6].1[3].ends_with("[fmt:SGRBG10_1X10/640x480]"));
    }
}
