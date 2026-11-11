//! BallsMaker — seeded geometric avatars. One regular shape (circle,
//! square, triangle, pentagon, hexagon, star, capsule, diamond) in
//! near-black on off-white, gooified (rasterize → blur → threshold →
//! marching-squares contour) so the corners melt, two stadium eye
//! cut-outs. Deterministic per seed, so `:::pfp:::` and
//! `verity bot pfp --seed` reproduce exactly.

use thiserror::Error;
use tiny_skia::*;

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

/// Regular n-gon as vertices around (c,c).
fn ngon(n: usize, c: f64, r: f64, rot: f64) -> Vec<(f64, f64)> {
    (0..n)
        .map(|i| {
            let a = rot + i as f64 * std::f64::consts::TAU / n as f64;
            (c + r * a.cos(), c + r * a.sin())
        })
        .collect()
}

/// 5-point star vertices around (c,c), outer r, inner 0.45·r.
fn star(c: f64, r: f64, rot: f64) -> Vec<(f64, f64)> {
    (0..10)
        .map(|i| {
            let rr = if i % 2 == 0 { r } else { r * 0.45 };
            let a = rot + i as f64 * std::f64::consts::TAU / 10.0;
            (c + rr * a.cos(), c + rr * a.sin())
        })
        .collect()
}

/// Stadium (capsule) vertex loop — two arcs joined by straights.
fn capsule(c: f64, w: f64, h: f64, tilt: f64) -> Vec<(f64, f64)> {
    let (r, ch) = (w / 2.0, h / 2.0 - w / 2.0);
    let (cs, sn) = (tilt.cos(), tilt.sin());
    (0..32)
        .map(|i| {
            let (a, sgn) = if i < 16 {
                (std::f64::consts::PI + i as f64 * std::f64::consts::PI / 16.0, -1.0)
            } else {
                ((i - 16) as f64 * std::f64::consts::PI / 16.0, 1.0)
            };
            let (x, y) = (r * a.cos(), sgn * ch + r * a.sin());
            (c + x * cs - y * sn, c + x * sn + y * cs)
        })
        .collect()
}

/// Render a BallsMaker avatar at `size`×`size`, return PNG bytes.
/// Near-black gooified shape on off-white, two stadium eye cut-outs.
pub fn render(seed: u64, size: u32) -> Result<Vec<u8>> {
    render_opts(seed, size, None)
}

/// [`render`] with a knob: `hue` = bg hue in degrees (None → off-white).
pub fn render_opts(seed: u64, size: u32, hue: Option<f64>) -> Result<Vec<u8>> {
    if size == 0 {
        return Err(PfpError::Other("pfp: size 0".into()));
    }
    let mut rng = Rng(seed.max(1));
    let s = size as f64;
    let c = s * 0.5;
    let bgc = match hue {
        Some(h) => hsl(h, 0.62, 0.5),
        None => (0xf9, 0xf9, 0xf9),
    };

    // seeded extent ~52–70% of the canvas, and a small tilt so even
    // symmetric shapes feel less rigid
    let r = s * rng.range(0.52, 0.7) / 2.0;
    let tilt = rng.range(-0.12, 0.12);
    let up = -std::f64::consts::FRAC_PI_2 + tilt;

    // every shape is a vertex loop (circles → 48-gons); circumradius
    // scaled so the bbox lands near 2r
    let verts = match rng.u(8) {
        0 => ngon(48, c, r, 0.0), // circle
        1 => ngon(4, c, r * 1.35, std::f64::consts::FRAC_PI_4 + tilt), // square
        2 => ngon(3, c, r * 1.16, up),                               // triangle
        3 => ngon(5, c, r * 1.05, up),                               // pentagon
        4 => ngon(6, c, r, tilt),                                    // hexagon
        5 => star(c, r * 1.1, up),                                   // star
        6 => capsule(c, r * 1.25, r * 2.0, tilt),                    // capsule
        _ => ngon(4, c, r * 1.41, tilt),                             // diamond
    };

    // rasterize → blur → threshold → trace — the metaball goo on a
    // regular shape, melted corners and all, re-vectorized
    const MW: usize = 288;
    let mut field = vec![0.0f32; MW * MW];
    rasterize(&verts, &mut field, MW, MW as f64 / s);
    blur(&mut field, MW, (MW as f32 * 0.045) as usize);
    let loops = trace(&field, MW);
    if loops.is_empty() {
        return Err(PfpError::Other("pfp: empty shape".into()));
    }

    let mut px =
        Pixmap::new(size, size).ok_or_else(|| PfpError::Other("pfp: pixmap alloc failed".into()))?;
    px.fill(Color::from_rgba8(bgc.0, bgc.1, bgc.2, 255));

    let mut paint = Paint::default();
    paint.anti_alias = true;
    let k = (s / MW as f64) as f32;

    // body — one path of all traced loops, near-black
    let mut pb = PathBuilder::new();
    for lp in &loops {
        pb.move_to(lp[0].0 as f32 * k, lp[0].1 as f32 * k);
        for &(x, y) in &lp[1..] {
            pb.line_to(x as f32 * k, y as f32 * k);
        }
        pb.close();
    }
    if let Some(p) = pb.finish() {
        paint.set_color_rgba8(0x0a, 0x0a, 0x0a, 255);
        px.fill_path(&p, &paint, FillRule::Winding, Transform::identity(), None);
    }

    // stadium eye cut-outs — painted bg colour over the shape
    let (gap, ew, eh, ey) = (s * 0.09, s * 0.05, s * 0.12, s * 0.5);
    let mut pb = PathBuilder::new();
    for ex in [c - gap / 2.0, c + gap / 2.0] {
        for (i, &(x, y)) in capsule(0.0, ew, eh, 0.0).iter().enumerate() {
            if i == 0 {
                pb.move_to((x + ex) as f32, (y + ey) as f32);
            } else {
                pb.line_to((x + ex) as f32, (y + ey) as f32);
            }
        }
        pb.close();
    }
    if let Some(p) = pb.finish() {
        paint.set_color_rgba8(bgc.0, bgc.1, bgc.2, 255);
        px.fill_path(&p, &paint, FillRule::Winding, Transform::identity(), None);
    }

    px.encode_png()
        .map_err(|e| PfpError::Other(format!("pfp: png encode: {e}")))
}

/// Fill `field` with 1.0 inside the polygon (even-odd ray cast),
/// vertices given in canvas coords scaled by `k` into grid units.
fn rasterize(v: &[(f64, f64)], field: &mut [f32], mw: usize, k: f64) {
    let v: Vec<(f64, f64)> = v.iter().map(|&(x, y)| (x * k, y * k)).collect();
    for y in 0..mw {
        for x in 0..mw {
            let (px, py) = (x as f64 + 0.5, y as f64 + 0.5);
            let mut inside = false;
            let mut j = v.len() - 1;
            for i in 0..v.len() {
                let (xi, yi) = v[i];
                let (xj, yj) = v[j];
                if (yi > py) != (yj > py) && px < (xj - xi) * (py - yi) / (yj - yi) + xi {
                    inside = !inside;
                }
                j = i;
            }
            field[y * mw + x] = if inside { 1.0 } else { 0.0 };
        }
    }
}

/// Three separable box-blur passes ≈ a Gaussian.
fn blur(f: &mut [f32], w: usize, r: usize) {
    if r == 0 {
        return;
    }
    let d = (2 * r + 1) as f32;
    let mut t = vec![0.0f32; f.len()];
    for _ in 0..3 {
        for y in 0..w {
            let mut acc: f32 = (0..=r.min(w - 1)).map(|x| f[y * w + x]).sum();
            for x in 0..w {
                t[y * w + x] = acc / d;
                let (a, b) = (x.wrapping_sub(r), x + r + 1);
                if b < w {
                    acc += f[y * w + b];
                }
                if x >= r {
                    acc -= f[y * w + a];
                } else if x == 0 {
                    acc -= f[y * w];
                }
            }
        }
        for x in 0..w {
            let mut acc: f32 = (0..=r.min(w - 1)).map(|y| t[y * w + x]).sum();
            for y in 0..w {
                f[y * w + x] = acc / d;
                let (a, b) = (y.wrapping_sub(r), y + r + 1);
                if b < w {
                    acc += t[b * w + x];
                }
                if y >= r {
                    acc -= t[a * w + x];
                } else if y == 0 {
                    acc -= t[x];
                }
            }
        }
    }
}

/// Marching squares: extract the iso-0.5 contour of the blurred
/// field as closed loops (grid coords).
fn trace(f: &[f32], mw: usize) -> Vec<Vec<(f64, f64)>> {
    use std::collections::HashMap;
    // edge ids: horizontal h(x,y) → y*mw+x; vertical v(x,y) → mw² + y*mw+x
    let mut segs: Vec<(usize, usize, (f64, f64), (f64, f64))> = vec![];
    let mut by_edge: HashMap<usize, Vec<usize>> = HashMap::new();
    let interp = |va: f32, vb: f32| ((0.5 - va) / (vb - va)).clamp(0.0, 1.0) as f64;
    let pts_of = |f: &[f32], e: usize| -> (f64, f64) {
        if e < mw * mw {
            let (ex, ey) = (e % mw, e / mw);
            (ex as f64 + interp(f[ey * mw + ex], f[ey * mw + ex + 1]), ey as f64)
        } else {
            let (ex, ey) = ((e - mw * mw) % mw, (e - mw * mw) / mw);
            (ex as f64, ey as f64 + interp(f[ey * mw + ex], f[(ey + 1) * mw + ex]))
        }
    };
    for y in 0..mw - 1 {
        for x in 0..mw - 1 {
            let (c0, c1) = (f[y * mw + x], f[y * mw + x + 1]);
            let (c3, c2) = (f[(y + 1) * mw + x], f[(y + 1) * mw + x + 1]);
            let case = (c0 > 0.5) as u8
                | ((c1 > 0.5) as u8) << 1
                | ((c2 > 0.5) as u8) << 2
                | ((c3 > 0.5) as u8) << 3;
            if case == 0 || case == 15 {
                continue;
            }
            let (top, right) = (y * mw + x, mw * mw + y * mw + x + 1);
            let (bot, left) = ((y + 1) * mw + x, mw * mw + y * mw + x);
            let ctr = (c0 + c1 + c2 + c3) / 4.0 > 0.5;
            let pairs: &[(usize, usize)] = match case {
                1 | 14 => &[(left, top)],
                2 | 13 => &[(top, right)],
                3 | 12 => &[(left, right)],
                4 | 11 => &[(right, bot)],
                5 => {
                    if ctr {
                        &[(left, top), (right, bot)]
                    } else {
                        &[(left, bot), (top, right)]
                    }
                }
                6 | 9 => &[(top, bot)],
                7 | 8 => &[(left, bot)],
                10 => {
                    if ctr {
                        &[(top, right), (left, bot)]
                    } else {
                        &[(top, left), (right, bot)]
                    }
                }
                _ => &[],
            };
            for &(a, b) in pairs {
                segs.push((a, b, pts_of(f, a), pts_of(f, b)));
                by_edge.entry(a).or_default().push(segs.len() - 1);
                by_edge.entry(b).or_default().push(segs.len() - 1);
            }
        }
    }
    // link segments into closed loops — walk from each unused
    // segment, following the edge adjacency until it dead-ends or
    // returns home
    let mut used = vec![false; segs.len()];
    let mut loops = vec![];
    for id in 0..segs.len() {
        if used[id] {
            continue;
        }
        used[id] = true;
        let mut loop_pts = vec![segs[id].2, segs[id].3];
        let mut edge = segs[id].1; // exit edge of the start segment
        while let Some(&nid) = by_edge
            .get(&edge)
            .and_then(|v| v.iter().find(|&&s| !used[s]))
        {
            used[nid] = true;
            let sg = segs[nid];
            let (exit_pt, exit_edge) = if sg.0 == edge { (sg.3, sg.1) } else { (sg.2, sg.0) };
            loop_pts.push(exit_pt);
            edge = exit_edge;
        }
        if loop_pts.len() >= 3 {
            loops.push(loop_pts);
        }
    }
    loops
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
