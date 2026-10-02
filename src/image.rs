//! Pure pixel work: raw unpacking, demosaic, white balance, colour conversion.
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

/// Sensor coordinate of one output column or row, plus its two neighbours
/// with the border mirrored so colour parity is preserved.
#[derive(Clone, Copy)]
struct Axis {
    c: usize,
    lo: usize,
    hi: usize,
}

/// One bayer row and the rows above and below it, and whether it is odd.
type Rows<'a> = (&'a [u8], &'a [u8], &'a [u8], bool);

/// Bilinear GRBG demosaic, one output pixel per sensor pixel, with white
/// balance, written straight to YUYV or RGB.
///
/// The source is a centred crop of the sensor: exact 1:1 when the output is
/// at most 16 px smaller than the sensor, otherwise the largest centred
/// region with the output's aspect ratio, sampled by nearest neighbour.
// ponytail: nearest-neighbour downscaling aliases, an area filter is the upgrade.
pub struct Debayer {
    width: usize,
    cols: Vec<Axis>,
    rows: Vec<Axis>,
    /// First sensor column of a 1:1 crop that never touches the left or
    /// right border, which lets `run_yuyv` take its fast path.
    crop_x: Option<usize>,
    /// Red and blue white-balance gains, /64 fixed point.
    pub wb: (u16, u16),
}

fn avg2(a: u8, b: u8) -> u32 {
    (a as u32 + b as u32 + 1) >> 1
}

fn avg4(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32 + b as u32 + c as u32 + d as u32 + 2) >> 2
}

/// Pack two horizontally adjacent RGB pixels as one YUYV group.
#[inline(always)]
fn put_yuyv(dst: &mut [u8], p0: [i32; 3], p1: [i32; 3]) {
    let luma = |[r, g, b]: [i32; 3]| (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
    let (r, g, b) = (
        (p0[0] + p1[0]) / 2,
        (p0[1] + p1[1]) / 2,
        (p0[2] + p1[2]) / 2,
    );
    dst[0] = luma(p0);
    dst[1] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
    dst[2] = luma(p1);
    dst[3] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
}

impl Debayer {
    /// `width`/`height` are the sensor's, and must both be at least 2.
    pub fn new(width: usize, height: usize, out_width: usize, out_height: usize) -> Self {
        let (rw, rh, x0, y0) = if out_width <= width
            && out_height <= height
            && width - out_width <= 16
            && height - out_height <= 16
        {
            (
                out_width,
                out_height,
                (width - out_width) / 2,
                (height - out_height) / 2,
            )
        } else {
            let (rw, rh) = if width * out_height >= height * out_width {
                ((height * out_width / out_height).clamp(1, width), height)
            } else {
                (width, (width * out_height / out_width).clamp(1, height))
            };
            (rw, rh, (width - rw) / 2, (height - rh) / 2)
        };
        // Centre of each output pixel's footprint, mapped into the region.
        let axis = |len: usize, region: usize, start: usize, out: usize| -> Vec<Axis> {
            (0..out)
                .map(|i| {
                    let c = start + (2 * i + 1) * region / (2 * out);
                    Axis {
                        c,
                        lo: if c == 0 { 1 } else { c - 1 },
                        hi: if c + 1 == len { len - 2 } else { c + 1 },
                    }
                })
                .collect()
        };
        let fast = rw == out_width && x0 >= 1 && x0 + out_width < width && out_width % 2 == 0;
        Self {
            width,
            crop_x: fast.then_some(x0),
            cols: axis(width, rw, x0, out_width),
            rows: axis(height, rh, y0, out_height),
            wb: (WB_UNITY, WB_UNITY),
        }
    }

    fn rows_of<'a>(&self, bayer: &'a [u8], ay: &Axis) -> Rows<'a> {
        let w = self.width;
        let row = |y: usize| &bayer[y * w..(y + 1) * w];
        (row(ay.lo), row(ay.c), row(ay.hi), ay.c & 1 == 1)
    }

    /// White-balanced (r, g, b) of the centre of a 3x3 neighbourhood, each
    /// row given as [left, centre, right]. The parities are constants at
    /// every call on the fast path, so the match folds away there.
    #[inline(always)]
    fn rgb(
        &self,
        odd_row: bool,
        odd_col: bool,
        up: [u8; 3],
        cur: [u8; 3],
        dn: [u8; 3],
    ) -> [i32; 3] {
        let (red, green, blue) = match (odd_row, odd_col) {
            (false, false) => (avg2(cur[0], cur[2]), cur[1] as u32, avg2(up[1], dn[1])),
            (true, true) => (avg2(up[1], dn[1]), cur[1] as u32, avg2(cur[0], cur[2])),
            (false, true) => (
                cur[1] as u32,
                avg4(cur[0], cur[2], up[1], dn[1]),
                avg4(up[0], up[2], dn[0], dn[2]),
            ),
            (true, false) => (
                avg4(up[0], up[2], dn[0], dn[2]),
                avg4(cur[0], cur[2], up[1], dn[1]),
                cur[1] as u32,
            ),
        };
        let red = (red * self.wb.0 as u32) >> 6;
        let blue = (blue * self.wb.1 as u32) >> 6;
        [red.min(255) as i32, green as i32, blue.min(255) as i32]
    }

    /// White-balanced (r, g, b) at one output column.
    fn pixel(&self, (up, cur, dn, odd_row): Rows, a: &Axis) -> [i32; 3] {
        let (x, l, r) = (a.c, a.lo, a.hi);
        self.rgb(
            odd_row,
            x & 1 == 1,
            [up[l], up[x], up[r]],
            [cur[l], cur[x], cur[r]],
            [dn[l], dn[x], dn[r]],
        )
    }

    /// One output row of a 1:1 crop. The row slices start one sensor pixel
    /// left of the first output pixel and end one right of the last.
    #[inline(always)]
    fn crop_row(&self, odd_row: bool, odd_col: bool, src: [&[u8]; 3], dst: &mut [u8]) {
        let quads = src[0]
            .windows(4)
            .step_by(2)
            .zip(src[1].windows(4).step_by(2))
            .zip(src[2].windows(4).step_by(2));
        for (d, ((u, c), n)) in dst.chunks_exact_mut(4).zip(quads) {
            let p0 = self.rgb(
                odd_row,
                odd_col,
                [u[0], u[1], u[2]],
                [c[0], c[1], c[2]],
                [n[0], n[1], n[2]],
            );
            let p1 = self.rgb(
                odd_row,
                !odd_col,
                [u[1], u[2], u[3]],
                [c[1], c[2], c[3]],
                [n[1], n[2], n[3]],
            );
            put_yuyv(d, p0, p1);
        }
    }

    /// Fill `out` (out_width * out_height * 2 bytes) with YUYV (YUY2), BT.601
    /// limited range. Chroma is averaged over each horizontal pixel pair, so
    /// the output width must be even.
    pub fn run_yuyv(&self, bayer: &[u8], out: &mut [u8]) {
        let stride = self.cols.len() * 2;
        let rows = out.chunks_exact_mut(stride).zip(&self.rows);
        if let Some(x0) = self.crop_x {
            let span = x0 - 1..x0 + self.cols.len() + 1;
            for (row, ay) in rows {
                let (up, cur, dn, odd_row) = self.rows_of(bayer, ay);
                let src = [&up[span.clone()], &cur[span.clone()], &dn[span.clone()]];
                // Spelled out so each parity gets its own branch-free loop.
                match (odd_row, x0 & 1 == 1) {
                    (false, false) => self.crop_row(false, false, src, row),
                    (false, true) => self.crop_row(false, true, src, row),
                    (true, false) => self.crop_row(true, false, src, row),
                    (true, true) => self.crop_row(true, true, src, row),
                }
            }
            return;
        }
        for (row, ay) in rows {
            let src = self.rows_of(bayer, ay);
            for (dst, pair) in row.chunks_exact_mut(4).zip(self.cols.chunks_exact(2)) {
                put_yuyv(dst, self.pixel(src, &pair[0]), self.pixel(src, &pair[1]));
            }
        }
    }

    /// Fill `out` (out_width * out_height * 3 bytes) with RGB pixels.
    pub fn run_rgb(&self, bayer: &[u8], out: &mut [u8]) {
        let stride = self.cols.len() * 3;
        for (row, ay) in out.chunks_exact_mut(stride).zip(&self.rows) {
            let src = self.rows_of(bayer, ay);
            for (dst, a) in row.chunks_exact_mut(3).zip(&self.cols) {
                let p = self.pixel(src, a);
                dst.copy_from_slice(&[p[0] as u8, p[1] as u8, p[2] as u8]);
            }
        }
    }
}

/// Fill a YUYV frame with black (Y=16, U=V=128), the placeholder while the
/// sensor is off.
pub fn fill_black_yuyv(yuyv: &mut [u8]) {
    for px in yuyv.chunks_exact_mut(4) {
        px.copy_from_slice(&[16, 128, 16, 128]);
    }
}

/// Header of a binary PPM (P6); the RGB bytes follow.
pub fn ppm_header(width: usize, height: usize) -> Vec<u8> {
    format!("P6\n{width} {height}\n255\n").into_bytes()
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

    #[test]
    fn crop_fast_path_matches_general_path() {
        // Even and odd crop offsets, so all four parity kernels run.
        for (w, h) in [(40, 30), (38, 28)] {
            let mut seed = 12345u32;
            let bayer: Vec<u8> = (0..w * h)
                .map(|_| {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    (seed >> 24) as u8
                })
                .collect();
            let mut d = Debayer::new(w, h, 32, 22);
            d.wb = (90, 110);
            assert!(d.crop_x.is_some());
            let mut fast = vec![0u8; 32 * 22 * 2];
            d.run_yuyv(&bayer, &mut fast);
            d.crop_x = None;
            let mut general = vec![0u8; 32 * 22 * 2];
            d.run_yuyv(&bayer, &mut general);
            assert_eq!(fast, general);
        }
    }

    #[test]
    #[ignore = "timing only: cargo test --release -- --ignored --nocapture"]
    fn bench_1080p() {
        let bayer = vec![100u8; 1928 * 1092];
        let mut out = vec![0u8; 1920 * 1080 * 2];
        let mut d = Debayer::new(1928, 1092, 1920, 1080);
        for fast in [true, false] {
            if !fast {
                d.crop_x = None;
            }
            let t = std::time::Instant::now();
            for _ in 0..50 {
                d.run_yuyv(std::hint::black_box(&bayer), &mut out);
            }
            println!("fast={fast}: {:?}/frame", t.elapsed() / 50);
        }
    }

    fn le_bytes(vals: &[u16]) -> Vec<u8> {
        vals.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn rgb_of(d: &Debayer, bayer: &[u8], n: usize) -> Vec<u8> {
        let mut out = vec![9u8; n * 3];
        d.run_rgb(bayer, &mut out);
        out
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
    fn solid_colour_is_uniform_including_borders() {
        let (w, h) = (16, 8);
        let bayer = solid_bayer(w, h, 60, 200, 20);
        let d = Debayer::new(w, h, w, h);
        assert_eq!(d.wb, (WB_UNITY, WB_UNITY));
        let want = [60u8, 200u8, 20u8];
        for px in rgb_of(&d, &bayer, w * h).chunks_exact(3) {
            assert_eq!(px, want);
        }
    }

    #[test]
    fn exact_crop_for_sensor_padding() {
        let d = Debayer::new(1928, 1092, 1920, 1080);
        assert_eq!((d.cols[0].c, d.rows[0].c), (4, 6));
        assert_eq!((d.cols[1919].c, d.rows[1079].c), (1923, 1085));
        assert!(d.cols.windows(2).all(|p| p[1].c == p[0].c + 1));
        let bayer = solid_bayer(1928, 1092, 10, 10, 10);
        let mut out = vec![0u8; 1920 * 1080 * 3];
        d.run_rgb(&bayer, &mut out);
        assert!(out.iter().all(|&v| v == 10));
    }

    #[test]
    fn other_sizes_use_centred_aspect_region() {
        // 4:3 into the 1928x1092 sensor: 1456 wide, centred at offset 236.
        let d = Debayer::new(1928, 1092, 640, 480);
        assert!(d.cols[0].c >= 236 && d.cols[639].c < 236 + 1456);
        assert!(d.rows[0].c < 3 && d.rows[479].c > 1088);
        // 16:9 is wider than the sensor allows: full width, 1084 high.
        let d = Debayer::new(1928, 1092, 1280, 720);
        assert!(d.cols[0].c < 3 && d.cols[1279].c > 1925);
        assert!(d.rows[0].c >= 4 && d.rows[719].c < 1088);
    }

    #[test]
    fn tiny_and_upscaled_outputs_do_not_panic() {
        let bayer = solid_bayer(2, 2, 1, 2, 3);
        let d = Debayer::new(2, 2, 1, 1);
        assert_eq!(rgb_of(&d, &bayer, 1).len(), 3);
        let d = Debayer::new(2, 2, 6, 4);
        let want = [1u8, 2u8, 3u8];
        for px in rgb_of(&d, &bayer, 24).chunks_exact(3) {
            assert_eq!(px, want);
        }
        let bayer = solid_bayer(8, 8, 1, 2, 3);
        let d = Debayer::new(8, 8, 1, 1);
        assert_eq!(rgb_of(&d, &bayer, 1), [1u8, 2u8, 3u8]);
        let d = Debayer::new(8, 8, 32, 2);
        let mut yuyv = vec![0u8; 32 * 2 * 2];
        d.run_yuyv(&bayer, &mut yuyv);
    }

    #[test]
    fn white_balance_applies_and_saturates() {
        let (w, h) = (4, 4);
        let bayer = solid_bayer(w, h, 100, 150, 200);
        let mut d = Debayer::new(w, h, w, h);
        d.wb = (96, 128);
        // R 100*96/64 = 150, B 200*128/64 = 400 saturates to 255
        let want = [150u8, 150u8, 255u8];
        assert_eq!(&rgb_of(&d, &bayer, 16)[..3], want);
    }

    #[test]
    fn bilinear_averages_neighbours() {
        // Pixel (0,0) is G on an even row: R from the right neighbour (the
        // left mirrors onto it), B from the row below (the row above mirrors).
        let bayer = [
            50, 100, 50, 100, 40, 50, 40, 50, 50, 100, 50, 100, 40, 50, 40, 50,
        ];
        let d = Debayer::new(4, 4, 4, 4);
        let out = rgb_of(&d, &bayer, 16);
        assert_eq!(&out[..3], [100u8, 50u8, 40u8]);
        // (1,0) is R: G is the average of 4 edges (50,50,50,50), B of diagonals.
        assert_eq!(&out[3..6], [100u8, 50u8, 40u8]);
    }

    fn yuyv_solid(r: u8, g: u8, b: u8) -> [u8; 4] {
        let bayer = solid_bayer(4, 4, r, g, b);
        let d = Debayer::new(4, 4, 2, 2);
        let mut out = vec![0u8; 8];
        d.run_yuyv(&bayer, &mut out);
        assert_eq!(out[..4], out[4..]);
        [out[0], out[1], out[2], out[3]]
    }

    fn near(a: u8, b: u8) -> bool {
        a.abs_diff(b) <= 1
    }

    #[test]
    fn yuyv_white_and_black() {
        let w = yuyv_solid(255, 255, 255);
        assert!(near(w[0], 235) && near(w[2], 235), "{w:?}");
        assert!(near(w[1], 128) && near(w[3], 128), "{w:?}");
        assert_eq!(yuyv_solid(0, 0, 0), [16, 128, 16, 128]);
    }

    #[test]
    fn yuyv_primaries_have_expected_chroma_direction() {
        let red = yuyv_solid(255, 0, 0);
        assert!(red[1] < 128 && red[3] > 128);
        let blue = yuyv_solid(0, 0, 255);
        assert!(blue[1] > 128 && blue[3] < 128);
    }

    #[test]
    fn black_fill_matches_converted_black_frame() {
        let (w, h) = (8, 4);
        let mut black = vec![0u8; w * h * 2];
        fill_black_yuyv(&mut black);
        let d = Debayer::new(w, h, w, h);
        let mut conv = vec![1u8; w * h * 2];
        d.run_yuyv(&vec![0u8; w * h], &mut conv);
        assert_eq!(black, conv);
    }

    #[test]
    fn ppm_header_format() {
        assert_eq!(ppm_header(2, 1), b"P6\n2 1\n255\n");
    }
}
