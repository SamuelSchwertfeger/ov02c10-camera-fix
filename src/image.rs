//! Pure pixel work: raw unpacking, debayer, white balance, colour conversion.
//! Nothing here touches hardware, so all of it is unit-testable anywhere.

/// Raw values at or above this (of 255) count as clipped.
pub const CLIP_LEVEL: u8 = 250;
/// White-balance gains are 6-bit fixed point: 64 is 1.0.
pub const WB_UNITY: u16 = 64;

/// Unpack a V4L2_PIX_FMT_SGRBG10 frame to an 8-bit Bayer mosaic.
///
/// Each pixel is a 16-bit little-endian value with 10 significant bits;
/// `>> 2` keeps the top 8. Rows are padded to `stride` bytes. Returns false
/// (leaving `out` untouched) if `raw` is too short for the geometry.
pub fn unpack_sgrbg10(
    raw: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    out: &mut [u8],
) -> bool {
    if stride < width * 2 || raw.len() < height * stride || out.len() < width * height {
        return false;
    }
    let rows = raw.chunks_exact(stride).zip(out.chunks_exact_mut(width));
    for (row, dst) in rows.take(height) {
        for (px, d) in row.chunks_exact(2).zip(dst.iter_mut()) {
            *d = (u16::from_le_bytes([px[0], px[1]]) >> 2).min(255) as u8;
        }
    }
    true
}

/// Mean brightness (0-255) and fraction of clipped pixels of a Bayer mosaic.
pub fn frame_stats(bayer: &[u8]) -> (f64, f64) {
    if bayer.is_empty() {
        return (0.0, 0.0);
    }
    let (mut sum, mut clipped) = (0u64, 0u64);
    for &p in bayer {
        sum += p as u64;
        clipped += (p >= CLIP_LEVEL) as u64;
    }
    let n = bayer.len() as f64;
    (sum as f64 / n, clipped as f64 / n)
}

/// Gray-world white balance: returns (red, blue) gains in /64 fixed point
/// that pull the R and B means onto the G mean.
///
/// 2x2 quads with any clipped channel are left out: a bright face is not
/// neutral gray and otherwise tints the whole background. If under 5% of
/// quads survive, the full frame is used instead. The ratio is clamped to
/// 0.5-3.0 so a genuinely non-neutral scene is not over-corrected.
pub fn white_balance(bayer: &[u8], width: usize, height: usize) -> (u16, u16) {
    let (qw, qh) = (width / 2, height / 2);
    let sums = |skip_clipped: bool| {
        let (mut r, mut g, mut b, mut n) = (0u64, 0u64, 0u64, 0u64);
        for qy in 0..qh {
            let top = &bayer[2 * qy * width..];
            let bot = &bayer[(2 * qy + 1) * width..];
            for qx in 0..qw {
                let (g1, rr) = (top[2 * qx], top[2 * qx + 1]);
                let (bb, g2) = (bot[2 * qx], bot[2 * qx + 1]);
                if skip_clipped && g1.max(rr).max(bb).max(g2) >= CLIP_LEVEL {
                    continue;
                }
                r += rr as u64;
                g += g1 as u64 + g2 as u64;
                b += bb as u64;
                n += 1;
            }
        }
        (r, g, b, n)
    };

    let total = (qw * qh) as f64;
    let mut s = sums(true);
    if (s.3 as f64) < total * 0.05 {
        s = sums(false);
    }
    let (r, g, b, n) = s;
    if n == 0 {
        return (WB_UNITY, WB_UNITY);
    }
    let n = n as f64;
    let (r_mean, g_mean, b_mean) = (r as f64 / n, g as f64 / (2.0 * n), b as f64 / n);
    let gain = |source: f64| {
        if source < 1.0 {
            WB_UNITY
        } else {
            ((g_mean / source).clamp(0.5, 3.0) * WB_UNITY as f64).round() as u16
        }
    };
    (gain(r_mean), gain(b_mean))
}

/// GRBG debayer straight to output resolution.
///
/// Each 2x2 quad (G at (0,0) and (1,1), R at (0,1), B at (1,0)) becomes one
/// half-resolution pixel; output pixels pick their quad by nearest neighbour.
// ponytail: box filter + nearest neighbour gives half the sensor's detail.
// Swap in a bilinear demosaic here if sharpness ever matters.
pub struct Debayer {
    width: usize,
    out_width: usize,
    out_height: usize,
    x_idx: Vec<usize>,
    y_idx: Vec<usize>,
    /// Red and blue white-balance gains, /64 fixed point.
    pub wb: (u16, u16),
}

impl Debayer {
    /// `width`/`height` are the sensor's, and must both be at least 2.
    pub fn new(width: usize, height: usize, out_width: usize, out_height: usize) -> Self {
        // Evenly spaced sensor coordinate for each output pixel, mapped to its quad.
        let index = |len: usize, out: usize| -> Vec<usize> {
            let quads = len / 2;
            (0..out)
                .map(|i| {
                    let pos = if out > 1 {
                        i * (len - 1) / (out - 1)
                    } else {
                        0
                    };
                    (pos / 2).min(quads - 1)
                })
                .collect()
        };
        Self {
            width,
            out_width,
            out_height,
            x_idx: index(width, out_width),
            y_idx: index(height, out_height),
            wb: (WB_UNITY, WB_UNITY),
        }
    }

    /// Fill `out` (out_width * out_height * 4 bytes) with BGRx pixels.
    pub fn run(&self, bayer: &[u8], out: &mut [u8]) {
        let (sr, sb) = (self.wb.0 as u32, self.wb.1 as u32);
        let rows = out.chunks_exact_mut(self.out_width * 4);
        for (row, &qy) in rows.take(self.out_height).zip(&self.y_idx) {
            let top = &bayer[2 * qy * self.width..];
            let bot = &bayer[(2 * qy + 1) * self.width..];
            for (px, &qx) in row.chunks_exact_mut(4).zip(&self.x_idx) {
                let g = (top[2 * qx] as u32 + bot[2 * qx + 1] as u32) / 2;
                let r = (top[2 * qx + 1] as u32 * sr) >> 6;
                let b = (bot[2 * qx] as u32 * sb) >> 6;
                px[0] = b.min(255) as u8;
                px[1] = g as u8;
                px[2] = r.min(255) as u8;
                px[3] = 0;
            }
        }
    }
}

/// BGRx to YUYV (YUY2), BT.601 limited range. Chroma is averaged over each
/// horizontal pixel pair, so the frame width must be even.
pub fn bgrx_to_yuyv(bgrx: &[u8], yuyv: &mut [u8]) {
    let luma = |r: i32, g: i32, b: i32| (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
    for (src, dst) in bgrx.chunks_exact(8).zip(yuyv.chunks_exact_mut(4)) {
        let (b0, g0, r0) = (src[0] as i32, src[1] as i32, src[2] as i32);
        let (b1, g1, r1) = (src[4] as i32, src[5] as i32, src[6] as i32);
        let (r, g, b) = ((r0 + r1) / 2, (g0 + g1) / 2, (b0 + b1) / 2);
        dst[0] = luma(r0, g0, b0);
        dst[1] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
        dst[2] = luma(r1, g1, b1);
        dst[3] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
    }
}

/// A black YUYV frame, used as the placeholder while the sensor is off.
pub fn black_yuyv(width: usize, height: usize) -> Vec<u8> {
    [16u8, 128, 16, 128].repeat(width * height / 2)
}

/// Encode a BGRx frame as a binary PPM (P6).
pub fn bgrx_to_ppm(bgrx: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut out = format!("P6\n{width} {height}\n255\n").into_bytes();
    out.reserve(width * height * 3);
    for px in bgrx.chunks_exact(4).take(width * height) {
        out.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Full-size GRBG mosaic where every quad has the same R/G/B.
    fn solid_bayer(w: usize, h: usize, r: u8, g: u8, b: u8) -> Vec<u8> {
        let mut v = vec![0; w * h];
        for y in 0..h {
            for x in 0..w {
                v[y * w + x] = match (y % 2, x % 2) {
                    (0, 1) => r,
                    (1, 0) => b,
                    _ => g,
                };
            }
        }
        v
    }

    fn le_bytes(vals: &[u16]) -> Vec<u8> {
        vals.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    #[test]
    fn unpack_shifts_10_bit_samples_to_8_bit() {
        let row0 = [0u16, 100, 200, 300, 400, 500, 600, 700];
        let row1 = [50u16, 150, 250, 350, 450, 550, 650, 750];
        let raw = le_bytes(&[row0, row1].concat());
        let mut out = vec![0u8; 16];
        assert!(unpack_sgrbg10(&raw, 8, 2, 16, &mut out));
        let want: Vec<u8> = [row0, row1]
            .concat()
            .iter()
            .map(|v| (v >> 2) as u8)
            .collect();
        assert_eq!(out, want);
    }

    #[test]
    fn unpack_skips_row_padding() {
        let mut raw = le_bytes(&[4, 8]);
        raw.extend_from_slice(&[0xFF; 4]);
        raw.extend(le_bytes(&[12, 16]));
        raw.extend_from_slice(&[0xFF; 4]);
        let mut out = vec![0u8; 4];
        assert!(unpack_sgrbg10(&raw, 2, 2, 8, &mut out));
        assert_eq!(out, [1, 2, 3, 4]);
    }

    #[test]
    fn unpack_saturates_and_rejects_short_buffers() {
        let raw = le_bytes(&[0xFFFF, 1023]);
        let mut out = vec![0u8; 2];
        assert!(unpack_sgrbg10(&raw, 2, 1, 4, &mut out));
        assert_eq!(out, [255, 255]);
        let mut out = vec![7u8; 2];
        assert!(!unpack_sgrbg10(&raw[..3], 2, 1, 4, &mut out));
        assert!(!unpack_sgrbg10(&raw, 2, 1, 2, &mut out));
        assert!(!unpack_sgrbg10(&raw, 2, 1, 4, &mut out[..1]));
        assert_eq!(out, [7, 7]);
    }

    #[test]
    fn frame_stats_mean_and_clipped_fraction() {
        assert_eq!(frame_stats(&[]), (0.0, 0.0));
        let (mean, clipped) = frame_stats(&[0, 0, 255, 250]);
        assert_eq!(mean, 126.25);
        assert_eq!(clipped, 0.5);
        let (_, clipped) = frame_stats(&[249; 4]);
        assert_eq!(clipped, 0.0);
    }

    #[test]
    fn white_balance_equalises_channels_to_green() {
        let bayer = solid_bayer(16, 16, 100, 150, 75);
        // sr = round(150/100*64) = 96, sb = round(150/75*64) = 128
        assert_eq!(white_balance(&bayer, 16, 16), (96, 128));
    }

    #[test]
    fn white_balance_clamps_extreme_ratios() {
        let bayer = solid_bayer(16, 16, 1, 200, 200);
        // 200x gain is clamped to 3.0x -> 192
        assert_eq!(white_balance(&bayer, 16, 16).0, 192);
        let bayer = solid_bayer(16, 16, 200, 100, 200);
        assert_eq!(white_balance(&bayer, 16, 16), (32, 32));
        let bayer = solid_bayer(16, 16, 0, 100, 0);
        assert_eq!(white_balance(&bayer, 16, 16), (WB_UNITY, WB_UNITY));
    }

    #[test]
    fn white_balance_excludes_clipped_regions() {
        let mut bayer = solid_bayer(16, 16, 100, 150, 75);
        bayer[..4 * 16].fill(255);
        assert_eq!(white_balance(&bayer, 16, 16), (96, 128));
    }

    #[test]
    fn white_balance_falls_back_to_full_frame_when_nearly_all_clipped() {
        let mut bayer = vec![255u8; 16 * 16];
        // one unclipped quad of 64 is 1.6%, under the 5% floor
        bayer[0] = 100;
        bayer[1] = 100;
        bayer[16] = 100;
        bayer[17] = 100;
        let (r, b) = white_balance(&bayer, 16, 16);
        assert!((60..=68).contains(&r) && (60..=68).contains(&b));
    }

    #[test]
    fn debayer_is_identity_at_unity_gain() {
        let (w, h) = (16, 8);
        let bayer = solid_bayer(w, h, 60, 200, 20);
        let d = Debayer::new(w, h, w, h);
        assert_eq!(d.wb, (WB_UNITY, WB_UNITY));
        let mut out = vec![9u8; w * h * 4];
        d.run(&bayer, &mut out);
        for px in out.chunks_exact(4) {
            assert_eq!(px, [20, 200, 60, 0]);
        }
    }

    #[test]
    fn debayer_output_matches_configured_resolution() {
        let (w, h) = (1928, 1092);
        let bayer = solid_bayer(w, h, 10, 10, 10);
        let d = Debayer::new(w, h, 320, 240);
        let mut out = vec![0u8; 320 * 240 * 4];
        d.run(&bayer, &mut out);
        assert!(out.chunks_exact(4).all(|p| p == [10, 10, 10, 0]));
    }

    #[test]
    fn debayer_upscales_and_handles_tiny_outputs_without_panicking() {
        let bayer = solid_bayer(2, 2, 1, 2, 3);
        let d = Debayer::new(2, 2, 6, 4);
        let mut out = vec![0u8; 6 * 4 * 4];
        d.run(&bayer, &mut out);
        assert!(out.chunks_exact(4).all(|p| p == [3, 2, 1, 0]));

        let bayer = solid_bayer(8, 8, 1, 2, 3);
        let d = Debayer::new(8, 8, 1, 1);
        let mut out = vec![0u8; 4];
        d.run(&bayer, &mut out);
        assert_eq!(out, [3, 2, 1, 0]);
    }

    #[test]
    fn debayer_applies_and_saturates_white_balance() {
        let (w, h) = (4, 4);
        let bayer = solid_bayer(w, h, 100, 150, 200);
        let mut d = Debayer::new(w, h, w, h);
        d.wb = (96, 128);
        let mut out = vec![0u8; w * h * 4];
        d.run(&bayer, &mut out);
        // R 100*96/64 = 150, B 200*128/64 = 400, saturates to 255
        assert_eq!(&out[..4], [255, 150, 150, 0]);
    }

    #[test]
    fn debayer_picks_quads_by_position() {
        // Left quad and right quad differ; a 2x1 output samples one each.
        let (w, h) = (4, 2);
        let bayer = [10, 200, 10, 20, 30, 10, 40, 10];
        let d = Debayer::new(w, h, 2, 1);
        let mut out = vec![0u8; 8];
        d.run(&bayer, &mut out);
        assert_eq!(&out[..4], [30, 10, 200, 0]);
        assert_eq!(&out[4..], [40, 10, 20, 0]);
    }

    fn yuyv_of(b: u8, g: u8, r: u8) -> [u8; 4] {
        let src = [b, g, r, 0, b, g, r, 0];
        let mut out = [0u8; 4];
        bgrx_to_yuyv(&src, &mut out);
        out
    }

    fn near(a: u8, b: u8) -> bool {
        a.abs_diff(b) <= 1
    }

    #[test]
    fn yuyv_white_and_black() {
        let w = yuyv_of(255, 255, 255);
        assert!(near(w[0], 235) && near(w[2], 235), "{w:?}");
        assert!(near(w[1], 128) && near(w[3], 128), "{w:?}");
        let k = yuyv_of(0, 0, 0);
        assert!(near(k[0], 16) && near(k[2], 16), "{k:?}");
        assert!(near(k[1], 128) && near(k[3], 128), "{k:?}");
    }

    #[test]
    fn yuyv_primaries_have_expected_chroma_direction() {
        let red = yuyv_of(0, 0, 255);
        assert!(red[1] < 128 && red[3] > 128);
        let blue = yuyv_of(255, 0, 0);
        assert!(blue[1] > 128 && blue[3] < 128);
    }

    #[test]
    fn black_yuyv_matches_converted_black_frame() {
        let (w, h) = (8, 4);
        let black = black_yuyv(w, h);
        assert_eq!(black.len(), w * h * 2);
        let mut conv = vec![0u8; w * h * 2];
        bgrx_to_yuyv(&vec![0u8; w * h * 4], &mut conv);
        for (a, b) in black.iter().zip(&conv) {
            assert!(near(*a, *b));
        }
    }

    #[test]
    fn ppm_has_header_and_rgb_order() {
        let bgrx = [1, 2, 3, 0, 4, 5, 6, 0];
        let ppm = bgrx_to_ppm(&bgrx, 2, 1);
        let header = b"P6\n2 1\n255\n";
        assert_eq!(&ppm[..header.len()], header);
        assert_eq!(&ppm[header.len()..], [3, 2, 1, 6, 5, 4]);
    }
}
