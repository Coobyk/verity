package sh.coobyk.verity.math

object TypstBridge {
    init { System.loadLibrary("verity_math") }
    /** Returns PNG bytes for the LaTeX [tex], or null on failure. */
    external fun render(tex: String, display: Boolean, density: Float): ByteArray?
}
