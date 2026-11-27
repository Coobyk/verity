#[test]
fn render_frac() {
    let png = verity_math::render_latex(r"x = \frac{-b \pm \sqrt{b^2 - 4ac}}{2a}", true, 3.0, true)
        .unwrap();
    std::fs::write("/tmp/math-newcm.png", &png).unwrap();
    let png = verity_math::render_latex(
        r"\int_0^\infty e^{-x^2} dx = \frac{\sqrt{\pi}}{2}",
        true,
        3.0,
        false,
    )
    .unwrap();
    std::fs::write("/tmp/math-newcm-black.png", &png).unwrap();
}
