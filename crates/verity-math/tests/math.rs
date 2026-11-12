#[test]
fn render_frac() {
    let png = verity_math::render_latex(r"\frac{a+b}{\sqrt{2}} + \int_0^\infty e^{-x^2} dx = \frac{\sqrt{\pi}}{2}", true, 3.0, true).unwrap();
    std::fs::write("/tmp/math-white.png", &png).unwrap();
    let png = verity_math::render_latex(r"\frac{a+b}{\sqrt{2}} + \int_0^\infty e^{-x^2} dx = \frac{\sqrt{\pi}}{2}", true, 3.0, false).unwrap();
    std::fs::write("/tmp/math-black.png", &png).unwrap();
    let inline = verity_math::render_latex(r"e^{i\pi} + 1 = 0", false, 3.0, true).unwrap();
    std::fs::write("/tmp/math-inline.png", &inline).unwrap();
}
