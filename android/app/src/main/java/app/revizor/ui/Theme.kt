package app.revizor.ui

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color

val Accent = Color(0xFF3DD6C6)
val Ink = Color(0xFF04201D)
val Bg = Color(0xFF0B0E13)
val Panel = Color(0xFF121720)
val Panel2 = Color(0xFF171D29)
val Line = Color(0xFF222B3A)
val Muted = Color(0xFF8793A6)
val Warn = Color(0xFFFFB454)
val Bad = Color(0xFFFF6B6B)
val Ok = Color(0xFF46D68C)

private val dark = darkColorScheme(
    primary = Accent, onPrimary = Ink, background = Bg, onBackground = Color(0xFFE8EDF5),
    surface = Panel, onSurface = Color(0xFFE8EDF5), surfaceVariant = Panel2, onSurfaceVariant = Muted,
    outline = Line, error = Bad,
)
private val light = lightColorScheme(
    primary = Color(0xFF0AA797), onPrimary = Color.White, background = Color(0xFFF4F6FA), surface = Color.White,
    surfaceVariant = Color(0xFFF0F3F8), onSurfaceVariant = Color(0xFF5D6B80), outline = Color(0xFFDDE3EE), error = Bad,
)

@Composable
fun RevizorTheme(content: @Composable () -> Unit) {
    // The receiver is a "screen": always dark there. Elsewhere follow the system.
    MaterialTheme(colorScheme = if (isSystemInDarkTheme()) dark else light, content = content)
}
