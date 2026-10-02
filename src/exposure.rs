//! Auto-exposure: decides the sensor's analogue gain from frame statistics.
//!
//! The raw V4L2 path has no AE/AGC of its own, so without this the sensor
//! sits at whatever gain it was started with while room lighting changes.

use crate::config::{GAIN_MAX, GAIN_MIN};

/// More than this fraction of clipped pixels triggers a gain cut even when
/// the frame average looks fine (a bright face against a dim room).
pub const CLIP_LIMIT: f64 = 0.10;

pub struct AutoExposure {
    pub gain: i32,
    target: f64,
    /// Set by a clip-triggered cut: the gain we cut to, and the brightness
    /// measured right after. Gain may not rise above it until the scene gets
    /// clearly darker; otherwise "average too dark, raise" and "highlights
    /// clipped, cut" would chase each other forever.
    ceiling: Option<(i32, Option<f64>)>,
}

impl AutoExposure {
    pub fn new(gain: i32, target: f64) -> Self {
        Self {
            gain,
            target,
            ceiling: None,
        }
    }

    /// Feed one measurement (mean brightness 0-255, clipped fraction 0-1).
    /// Returns the new gain to program into the sensor, if it should change.
    /// The caller must let the sensor settle before measuring again.
    pub fn step(&mut self, mean: f64, clipped: f64) -> Option<i32> {
        if clipped > CLIP_LIMIT {
            // Step harder when clipping is severe.
            let factor = if clipped > 0.5 { 0.5 } else { 0.7 };
            let cut = ((self.gain as f64 * factor).round() as i32).max(GAIN_MIN);
            if cut == self.gain {
                return None; // already at the floor
            }
            self.ceiling = Some((cut, None));
            self.gain = cut;
            return Some(cut);
        }

        if let Some((cap, reference)) = self.ceiling {
            match reference {
                None => self.ceiling = Some((cap, Some(mean))),
                Some(r) if mean < r * 0.6 => self.ceiling = None,
                Some(_) => {}
            }
        }

        // Acceptable band: 90-170 around the default target of 128.
        let (low, high) = (self.target * 90.0 / 128.0, self.target * 170.0 / 128.0);
        if (low..=high).contains(&mean) {
            return None;
        }
        let mut wanted = (self.gain as f64 * self.target / mean.max(1.0)).round() as i32;
        if let Some((cap, _)) = self.ceiling {
            wanted = wanted.min(cap);
        }
        let wanted = wanted.clamp(GAIN_MIN, GAIN_MAX);
        if wanted == self.gain {
            return None;
        }
        self.gain = wanted;
        Some(wanted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_band_brightness_changes_nothing() {
        let mut ae = AutoExposure::new(150, 128.0);
        assert_eq!(ae.step(128.0, 0.0), None);
        assert_eq!(ae.step(90.0, 0.0), None);
        assert_eq!(ae.step(170.0, 0.0), None);
        assert_eq!(ae.gain, 150);
    }

    #[test]
    fn dark_frame_raises_gain() {
        let mut ae = AutoExposure::new(100, 128.0);
        assert_eq!(ae.step(64.0, 0.0), Some(200));
        assert_eq!(ae.gain, 200);
    }

    #[test]
    fn bright_frame_lowers_gain() {
        let mut ae = AutoExposure::new(150, 128.0);
        // 150 * 128 / 236 = 81.4
        assert_eq!(ae.step(236.0, 0.0), Some(81));
    }

    #[test]
    fn gain_clamps_to_sensor_range() {
        let mut ae = AutoExposure::new(150, 128.0);
        assert_eq!(ae.step(2.0, 0.0), Some(GAIN_MAX));
        assert_eq!(ae.step(2.0, 0.0), None);
        let mut ae = AutoExposure::new(20, 128.0);
        assert_eq!(ae.step(255.0, 0.0), Some(GAIN_MIN));
        assert_eq!(ae.step(255.0, 0.0), None);
    }

    #[test]
    fn clipping_cuts_gain_even_when_average_is_fine() {
        let mut ae = AutoExposure::new(150, 128.0);
        assert_eq!(ae.step(99.0, 0.2), Some(105));
    }

    #[test]
    fn severe_clipping_halves_gain() {
        let mut ae = AutoExposure::new(150, 128.0);
        assert_eq!(ae.step(161.0, 0.6), Some(75));
        // 75 * 0.7 = 52.5 rounds away from zero here (Python gave 52)
        assert_eq!(ae.step(100.0, 0.2), Some(53));
    }

    #[test]
    fn clip_cut_stops_at_floor() {
        let mut ae = AutoExposure::new(GAIN_MIN, 128.0);
        assert_eq!(ae.step(100.0, 0.9), None);
        assert_eq!(ae.gain, GAIN_MIN);
    }

    #[test]
    fn ceiling_blocks_reraise_after_clip_cut() {
        let mut ae = AutoExposure::new(150, 128.0);
        assert_eq!(ae.step(100.0, 0.2), Some(105));
        // Average now looks dark, but raising would re-clip.
        assert_eq!(ae.step(60.0, 0.0), None);
        assert_eq!(ae.step(58.0, 0.0), None);
        assert_eq!(ae.gain, 105);
    }

    #[test]
    fn ceiling_lifts_when_scene_gets_clearly_darker() {
        let mut ae = AutoExposure::new(150, 128.0);
        assert_eq!(ae.step(100.0, 0.2), Some(105));
        assert_eq!(ae.step(60.0, 0.0), None); // reference = 60
        assert_eq!(ae.step(20.0, 0.0), Some(GAIN_MAX));
    }

    #[test]
    fn ceiling_still_allows_lowering() {
        let mut ae = AutoExposure::new(150, 128.0);
        assert_eq!(ae.step(100.0, 0.2), Some(105));
        assert_eq!(ae.step(60.0, 0.0), None);
        assert_eq!(ae.step(250.0, 0.0), Some(54));
    }
}
