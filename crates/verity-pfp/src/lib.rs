//! BallsMaker — seeded metaball-blob avatars. Rust port of
//! ballsmaker.pages.dev: scatter 4–7 balls, blur, hard alpha-threshold
//! (the "goo" filter) so they fuse into one blob, then drop auto-placed
//! capsule eyes on it. Deterministic per seed, so `:::pfp:::` and
//! `verity bot pfp --seed` reproduce exactly.

use thiserror::Error;

type Result<T> = std::result::Result<T, PfpError>;

#[derive(Debug, Error)]
pub enum PfpError {
    #[error("{0}")]
    Other(String),
}

impl From<PfpError> for std::io::Error {
    fn from(e: PfpError) -> Self {
        std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
    }
}
use tiny_skia::*;

/// xorshift64* — small, fast, plenty for art.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn f(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + self.f() * (hi - lo)
    }
    fn u(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// HSL → 8-bit RGB. Hues wrap, s/l clamp.
fn hsl(h: f64, s: f64, l: f64) -> (u8, u8, u8) {
    let h = h.rem_euclid(360.0) / 60.0;
    let s = s.clamp(0.0, 1.0);
    let l = l.clamp(0.0, 1.0);
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match h as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    )
}

/// One horizontal + vertical box-blur pass on an alpha buffer.
fn box_blur(a: &mut [u8], w: usize, h: usize, r: usize, tmp: &mut [u8]) {
    let n = 2 * r + 1;
    for y in 0..h {
        let row = y * w;
        let mut acc: u32 = 0;
        for x in 0..w {
            let add = a[row + (x + r).min(w - 1)] as u32;
            let sub = if x > r { a[row + x - r - 1] as u32 } else { 0 };
            acc += add;
            acc = acc.saturating_sub(sub);
            tmp[row + x] = (acc / n as u32) as u8;
        }
    }
    for x in 0..w {
        let mut acc: u32 = 0;
        for y in 0..h {
            let add = tmp[(y + r).min(h - 1) * w + x] as u32;
            let sub = if y > r {
                tmp[(y - r - 1) * w + x] as u32
            } else {
                0
            };
            acc += add;
            acc = acc.saturating_sub(sub);
            a[y * w + x] = (acc / n as u32) as u8;
        }
    }
}

/// Render a BallsMaker blob at `size`×`size`, return PNG bytes.
/// Faithful to ballsmaker.pages.dev: black goo blob, white stadium
/// eyes, white bg. `hue` swaps in a vivid background.
pub fn render(seed: u64, size: u32) -> Result<Vec<u8>> {
    render_opts(seed, size, None)
}

/// [`render`] with a knob: `hue` = bg hue in degrees (None → white).
pub fn render_opts(seed: u64, size: u32, hue: Option<f64>) -> Result<Vec<u8>> {
    let mut rng = Rng(seed.max(1));
    let s = size as f32;
    let k = s / 900.0; // the original canvas is 900×900

    // 1. Ball scatter — port of the site's randomBlob(): center ±120,
    //    4–7 balls at random angle/distance ≤170 (y squashed 0.75),
    //    radius 50–140.
    let cx = s * 0.5 + (rng.f() as f32 - 0.5) * 120.0 * k;
    let cy = s * 0.5 + (rng.f() as f32 - 0.5) * 120.0 * k;
    let n = 4 + rng.u(4);
    let mut balls = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let a = rng.range(0.0, std::f64::consts::TAU) as f32;
        let d = rng.range(0.0, 170.0) as f32 * k;
        balls.push((
            cx + a.cos() * d,
            cy + a.sin() * d * 0.75,
            rng.range(50.0, 140.0) as f32 * k,
        ));
    }

    // 2. Alpha mask at 2× — white circles → 3× box blur ≈ the
    //    feGaussianBlur (blur 28 → σ14 at 900px) → feColorMatrix hard
    //    threshold at 0.5 → downsample 2× for smooth goo edges.
    let w = size as usize;
    let mw = w * 2;
    let mut alpha = vec![0u8; mw * mw];
    {
        let mut px = Pixmap::new((size * 2).max(1), (size * 2).max(1))
            .ok_or_else(|| PfpError::Other("pfp: pixmap alloc failed".into()))?;
        let mut paint = Paint::default();
        paint.set_color_rgba8(255, 255, 255, 255);
        paint.anti_alias = false;
        for &(x, y, r) in &balls {
            let mut pb = PathBuilder::new();
            pb.push_circle(x * 2.0, y * 2.0, r * 2.0);
            if let Some(p) = pb.finish() {
                px.fill_path(&p, &paint, FillRule::Winding, Transform::identity(), None);
            }
        }
        for (i, p) in px.pixels().iter().enumerate() {
            alpha[i] = p.alpha();
        }
    }
    let mut tmp = vec![0u8; mw * mw];
    let br = (28.0 * k).max(4.0) as usize;
    for _ in 0..3 {
        box_blur(&mut alpha, mw, mw, br, &mut tmp);
    }

    // 3. Threshold in supersampled space → bbox/centroid for eyes;
    //    2× downsample keeps a soft coverage edge.
    let mut min_x = mw;
    let mut max_x = 0usize;
    let mut min_y = mw;
    let mut max_y = 0usize;
    let (mut sx, mut sy, mut count) = (0u64, 0u64, 0u64);
    for y in 0..mw {
        for x in 0..mw {
            let on = alpha[y * mw + x] >= 128;
            alpha[y * mw + x] = if on { 255 } else { 0 };
            if on {
                sx += x as u64;
                sy += y as u64;
                count += 1;
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
        }
    }
    if count == 0 {
        return Err(PfpError::Other("pfp: empty blob".into()));
    }
    let mut cover = vec![0u8; w * w];
    for y in 0..w {
        for x in 0..w {
            let i = y * 2 * mw + x * 2;
            let a = (alpha[i] as u16
                + alpha[i + 1] as u16
                + alpha[i + mw] as u16
                + alpha[i + mw + 1] as u16)
                / 4;
            cover[y * w + x] = a as u8;
        }
    }
    let (bcx, bcy) = (
        sx as f32 / count as f32 / 2.0,
        sy as f32 / count as f32 / 2.0,
    );
    let (bw, bh) = ((max_x - min_x) as f32 / 2.0, (max_y - min_y) as f32 / 2.0);

    // 4. Composite: white bg (the site's default) or a vivid hue →
    //    pure-black blob, matching the export's fillStyle "#000".
    let mut px = Pixmap::new(size, size)
        .ok_or_else(|| PfpError::Other("pfp: pixmap alloc failed".into()))?;
    let (br_, bg_, bb_) = match hue {
        Some(h) => hsl(h, 0.62, 0.5),
        None => (255, 255, 255),
    };
    px.fill(Color::from_rgba8(br_, bg_, bb_, 255));
    for (i, cov) in cover.iter().enumerate() {
        let c = *cov as u16;
        if c == 0 {
            continue;
        }
        // black blob over bg at coverage c → bg * (1 - c/255)
        let dst = px.pixels()[i];
        px.pixels_mut()[i] = PremultipliedColorU8::from_rgba(
            (dst.red() as u16 * (255 - c) / 255) as u8,
            (dst.green() as u16 * (255 - c) / 255) as u8,
            (dst.blue() as u16 * (255 - c) / 255) as u8,
            dst.alpha(),
        )
        .unwrap_or(dst);
    }

    // 5. Auto eyes — port of placeAutoEyes(): gap ≥ 46 or 30% of blob
    //    width, size 13% of blob height clamped 12–90, capsule w×1.5w
    //    centered slightly above the centroid.
    let gap = (46.0 * k).max(bw * 0.3);
    let esz = (bh * 0.13).clamp(12.0 * k, 90.0 * k);
    let (ew, eh) = (esz, esz * 1.5);
    let ey = bcy - bh * 0.06;
    let mut eye = Paint::default();
    eye.set_color_rgba8(255, 255, 255, 255);
    eye.anti_alias = true;
    for ex in [bcx - gap / 2.0, bcx + gap / 2.0] {
        // Eye capsule — a stadium (rect + two cap discs), matching the
        // site's roundRect. A plain oval reads egg-shaped, not right.
        let half = (eh - ew) / 2.0;
        if let Some(rect) = Rect::from_xywh(ex - ew / 2.0, ey - half, ew, half * 2.0) {
            let mut pb = PathBuilder::new();
            pb.push_rect(rect);
            if let Some(p) = pb.finish() {
                px.fill_path(&p, &eye, FillRule::Winding, Transform::identity(), None);
            }
        }
        for cy in [ey - half, ey + half] {
            let mut pb = PathBuilder::new();
            pb.push_circle(ex, cy, ew / 2.0);
            if let Some(p) = pb.finish() {
                px.fill_path(&p, &eye, FillRule::Winding, Transform::identity(), None);
            }
        }
    }

    px.encode_png()
        .map_err(|e| PfpError::Other(format!("pfp: png encode: {e}")))
}

/// Stable seed for a bot name — same name, same face, unless an
/// explicit seed overrides it.
pub fn name_seed(name: &str) -> u64 {
    // FNV-1a
    let mut h: u64 = 0xcbf29ce484222325;
    for b in name.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_png() {
        let png = render(42, 256).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert!(png.len() > 500);
    }

    #[test]
    fn deterministic_per_seed() {
        assert_eq!(render(7, 128).unwrap(), render(7, 128).unwrap());
        assert_ne!(render(7, 128).unwrap(), render(8, 128).unwrap());
    }
}
