package app.revizor.ui

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

@Composable
fun SectionTitle(text: String, modifier: Modifier = Modifier) {
    Text(text.uppercase(), modifier = modifier.padding(top = 22.dp, bottom = 8.dp), fontSize = 12.sp, fontWeight = FontWeight.SemiBold,
        letterSpacing = 1.6.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
}

@Composable
fun RowCard(title: String, subtitle: String?, selected: Boolean = false, trailing: @Composable (() -> Unit)? = null, onClick: () -> Unit) {
    Surface(
        onClick = onClick, shape = RoundedCornerShape(14.dp), color = MaterialTheme.colorScheme.surface,
        border = BorderStroke(if (selected) 1.5.dp else 1.dp, if (selected) MaterialTheme.colorScheme.primary else MaterialTheme.colorScheme.outline),
        modifier = Modifier.fillMaxWidth().padding(vertical = 4.dp),
    ) {
        Row(Modifier.padding(14.dp), verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            Column(Modifier.weight(1f)) {
                Text(title, fontWeight = FontWeight.SemiBold, maxLines = 1, overflow = TextOverflow.Ellipsis)
                if (subtitle != null) Text(subtitle, fontSize = 13.sp, color = MaterialTheme.colorScheme.onSurfaceVariant, maxLines = 2)
            }
            trailing?.invoke()
        }
    }
}

@Composable
fun Tag(text: String, color: Color = MaterialTheme.colorScheme.onSurfaceVariant) {
    Surface(shape = RoundedCornerShape(50), color = Color.Transparent, border = BorderStroke(1.dp, color.copy(alpha = 0.5f))) {
        Text(text, modifier = Modifier.padding(horizontal = 9.dp, vertical = 3.dp), fontSize = 11.sp, color = color)
    }
}

@Composable
fun StatTile(label: String, value: String, modifier: Modifier = Modifier) {
    Surface(shape = RoundedCornerShape(12.dp), color = MaterialTheme.colorScheme.surfaceVariant, modifier = modifier) {
        Column(Modifier.padding(horizontal = 12.dp, vertical = 10.dp)) {
            Text(label.uppercase(), fontSize = 10.sp, letterSpacing = 0.8.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
            Text(value, fontSize = 17.sp, fontWeight = FontWeight.SemiBold)
        }
    }
}

@Composable
fun Notice(text: String, bad: Boolean = false) {
    val c = if (bad) Bad else Warn
    Surface(shape = RoundedCornerShape(10.dp), color = c.copy(alpha = 0.12f), border = BorderStroke(1.dp, c.copy(alpha = 0.4f)), modifier = Modifier.fillMaxWidth().padding(vertical = 4.dp)) {
        Text(text, modifier = Modifier.padding(12.dp), color = c, fontSize = 13.5.sp)
    }
}

@Composable
fun StatusDot(color: Color) {
    Surface(color = color, shape = CircleShape, modifier = Modifier.size(9.dp)) {}
}

@Composable
fun Gap(h: Int = 12) = Spacer(Modifier.height(h.dp))

/** 2-column grid of tiles. */
@Composable
fun TileGrid(items: List<Pair<String, String>>) {
    items.chunked(2).forEach { pair ->
        Row(Modifier.fillMaxWidth().padding(vertical = 3.dp), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            pair.forEach { StatTile(it.first, it.second, Modifier.weight(1f)) }
            if (pair.size == 1) Spacer(Modifier.weight(1f))
        }
    }
}
