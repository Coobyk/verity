package sh.coobyk.verity.ui

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color

// Grok/X-style palette.
val VerityBlack = Color(0xFF000000)
val Hairline = Color(0xFF2F3336)
val DimText = Color(0xFF71767B)
val MainText = Color(0xFFE7E9EA)
val BubbleFill = Color(0xFF2F3336)
val ComposerFill = Color(0xFF16181C)
val ChosenChip = Color(0xFF20304A)
val ErrorRed = Color(0xFFF4212E)

private val scheme = darkColorScheme(
    background = VerityBlack,
    surface = VerityBlack,
    onBackground = MainText,
    onSurface = MainText,
    surfaceVariant = ComposerFill,
    onSurfaceVariant = DimText,
    primary = MainText,
    onPrimary = VerityBlack,
    secondary = DimText,
    outline = Hairline,
)

@Composable
fun VerityTheme(content: @Composable () -> Unit) {
    MaterialTheme(colorScheme = scheme, content = content)
}
