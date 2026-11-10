//! BallsMaker — seeded generative avatars: a scatter of lit spheres
//! ("balls") over a deep radial backdrop. Ported from the original
//! JS generator as a pure-Rust tiny-skia renderer — deterministic per
//! seed, so `:::pfp:::` and `verity bot pfp --seed` reproduce exactly.

use crate::{Error, Result};
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

fn circle(cx: f32, cy: f32, r: f32) -> Path {
    let mut pb = PathBuilder::new();
    pb.push_circle(cx, cy, r);
    pb.finish().expect("circle path")
}

/// Render a BallsMaker avatar at `size`×`size`, return PNG bytes.
/// Layout: deep radial backdrop, then ~20 spheres sorted back→front —
/// each a radial gradient from an off-center highlight to a dark rim,
/// plus a crisp specular dot. Two accent hues off the base hue keep
/// the palette tight but lively.
pub fn render(seed: u64, size: u32) -> Result<Vec<u8>> {
    let mut rng = Rng(seed.max(1));
    let s = size as f32;
    let mut px =
        Pixmap::new(size, size).ok_or_else(|| Error::Other("pfp: pixmap alloc failed".into()))?;

    // Backdrop: radial glow from upper-left, base hue, dark rim.
    let base = rng.range(0.0, 360.0);
    {
        let (r1, g1, b1) = hsl(base, 0.55, 0.22);
        let (r2, g2, b2) = hsl(base + 30.0, 0.6, 0.08);
        let stops = vec![
            GradientStop::new(0.0, Color::from_rgba8(r1, g1, b1, 255)),
            GradientStop::new(1.0, Color::from_rgba8(r2, g2, b2, 255)),
        ];
        if let Some(sh) = RadialGradient::new(
            Point::from_xy(s * 0.3, s * 0.25),
            Point::from_xy(s * 0.5, s * 0.55),
            s * 0.85,
            stops,
            SpreadMode::Pad,
            Transform::identity(),
        ) {
            let mut paint = Paint::default();
            paint.shader = sh;
            px.fill_rect(
                Rect::from_xywh(0.0, 0.0, s, s).unwrap(),
                &paint,
                Transform::identity(),
                None,
            );
        }
    }

    // Hue roster: base + two accents, ± jitter per ball.
    let hues = [
        base,
        base + rng.range(100.0, 160.0),
        base + rng.range(200.0, 260.0),
        base + 180.0,
    ];

    // Balls, far→near: distant ones smaller, dimmer, more transparent.
    let count = 16 + rng.u(8) as usize;
    let mut balls: Vec<(f32, f32, f32, usize, f64, f64)> = (0..count)
        .map(|_| {
            (
                rng.range(0.0, 1.0) as f32,
                rng.range(0.0, 1.0) as f32,
                rng.range(0.03, 0.30) as f32,
                rng.u(hues.len() as u64) as usize,
                rng.range(0.45, 1.0),
                rng.range(-25.0, 25.0),
            )
        })
        .collect();
    // Larger on top — sort by radius ascending.
    balls.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap());

    for (fx, fy, fr, hi, alpha, hj) in balls {
        let (cx, cy, r) = (fx * s, fy * s, fr * s);
        let hue = hues[hi] + hj;
        // Sphere shading: light source upper-left of the ball.
        let hx = cx - r * 0.35;
        let hy = cy - r * 0.35;
        let (lr, lg, lb) = hsl(hue, 0.75, 0.72);
        let (mr, mg, mb) = hsl(hue, 0.8, 0.5);
        let (dr, dg, db) = hsl(hue, 0.85, 0.22);
        let a = (alpha * 255.0) as u8;
        let stops = vec![
            GradientStop::new(0.0, Color::from_rgba8(lr, lg, lb, a)),
            GradientStop::new(0.45, Color::from_rgba8(mr, mg, mb, a)),
            GradientStop::new(1.0, Color::from_rgba8(dr, dg, db, a)),
        ];
        let Some(sh) = RadialGradient::new(
            Point::from_xy(hx, hy),
            Point::from_xy(cx, cy),
            r,
            stops,
            SpreadMode::Pad,
            Transform::identity(),
        ) else {
            continue;
        };
        let mut paint = Paint::default();
        paint.shader = sh;
        paint.anti_alias = true;
        px.fill_path(
            &circle(cx, cy, r),
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );

        // Specular dot — the "ball" read comes from this.
        let dot_r = r * 0.11;
        let mut dot = Paint::default();
        dot.set_color_rgba8(255, 255, 255, (alpha * 220.0) as u8);
        dot.anti_alias = true;
        px.fill_path(
            &circle(cx - r * 0.32, cy - r * 0.32, dot_r),
            &dot,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }

    // Vignette: dark rim keeps it readable as a tiny circle crop.
    {
        let stops = vec![
            GradientStop::new(0.0, Color::from_rgba8(0, 0, 0, 0)),
            GradientStop::new(1.0, Color::from_rgba8(0, 0, 0, 110)),
        ];
        if let Some(sh) = RadialGradient::new(
            Point::from_xy(s * 0.5, s * 0.5),
            Point::from_xy(s * 0.5, s * 0.5),
            s * 0.72,
            stops,
            SpreadMode::Pad,
            Transform::identity(),
        ) {
            let mut paint = Paint::default();
            paint.shader = sh;
            px.fill_rect(
                Rect::from_xywh(0.0, 0.0, s, s).unwrap(),
                &paint,
                Transform::identity(),
                None,
            );
        }
    }

    px.encode_png()
        .map_err(|e| Error::Other(format!("pfp: png encode: {e}")))
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
        assert!(png.len() > 1000);
    }

    #[test]
    fn deterministic_per_seed() {
        assert_eq!(render(7, 128).unwrap(), render(7, 128).unwrap());
        assert_ne!(render(7, 128).unwrap(), render(8, 128).unwrap());
    }
}
