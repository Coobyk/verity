// render <seed> <size> <hue|-1> <out.png>
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let seed: u64 = a[1].parse().unwrap();
    let size: u32 = a[2].parse().unwrap();
    let hue: f64 = a[3].parse().unwrap();
    let png =
        verity_pfp::render_opts(seed, size, if hue < 0.0 { None } else { Some(hue) }).unwrap();
    std::fs::write(&a[4], png).unwrap();
}
