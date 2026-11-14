//! ballsmaker — BallsMaker avatars from the shell.
//!
//!   ballsmaker random [out.png]        random face, random hue
//!   ballsmaker <seed> [out.png]        replay a face (hue = seed % 360)
//!   ballsmaker <seed> <hue> [out.png]  hue in degrees; -1 = near-black
//!
//! `--size N` sets the render size (default 512). Default out is
//! `ballsmaker-<seed>.png` in the cwd; `-` writes the PNG to stdout.

use std::process::ExitCode;

fn rand_u64() -> u64 {
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let mut b = [0u8; 8];
        if f.read_exact(&mut b).is_ok() {
            return u64::from_le_bytes(b);
        }
    }
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9e3779b97f4a7c15);
    n ^ (std::process::id() as u64).wrapping_mul(0x2545F4914F6CDD1D)
}

fn usage() -> ExitCode {
    eprintln!(
        "usage: ballsmaker [--size N] random|<seed> [hue] [out.png]\n\
         \x20      hue in degrees, -1 = near-black; out `-` = stdout"
    );
    ExitCode::FAILURE
}

fn main() -> ExitCode {
    let mut size: u32 = 512;
    let mut pos: Vec<String> = vec![];
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--size" | "-s" => match it.next().and_then(|v| v.parse().ok()) {
                Some(n) => size = n,
                None => return usage(),
            },
            "-h" | "--help" => return usage(),
            _ => pos.push(a),
        }
    }

    let Some(what) = pos.first() else {
        return usage();
    };
    let (seed, mut hue) = if what == "random" {
        (rand_u64().max(1), Some((rand_u64() % 360) as f64))
    } else {
        match what.parse::<u64>() {
            Ok(s) => (s, Some((s % 360) as f64)),
            Err(_) => return usage(),
        }
    };

    // optional hue — only when it parses as a number
    let rest = &pos[1..];
    let mut idx = 0;
    if let Some(Ok(h)) = rest.first().map(|a| a.parse::<f64>()) {
        hue = if h < 0.0 { None } else { Some(h) };
        idx = 1;
    }
    let out = rest.get(idx).cloned();

    let png = match verity_pfp::render_opts(seed, size, hue) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("ballsmaker: {e}");
            return ExitCode::FAILURE;
        }
    };

    let out = out.unwrap_or_else(|| format!("ballsmaker-{seed}.png"));
    let r = if out == "-" {
        std::io::Write::write_all(&mut std::io::stdout(), &png)
    } else {
        std::fs::write(&out, &png)
    };
    if let Err(e) = r {
        eprintln!("ballsmaker: {e}");
        return ExitCode::FAILURE;
    }
    match hue {
        Some(h) if out != "-" => println!("{out} (seed {seed}, hue {h:.0}°)"),
        None if out != "-" => println!("{out} (seed {seed}, near-black)"),
        _ => {}
    }
    ExitCode::SUCCESS
}
