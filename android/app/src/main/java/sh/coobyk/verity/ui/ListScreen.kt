package sh.coobyk.verity.ui

import android.graphics.BitmapFactory
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.produceState
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import sh.coobyk.verity.data.Conversation
import sh.coobyk.verity.data.ConversationKind

@OptIn(ExperimentalFoundationApi::class)
@Composable
fun ListScreen(vm: AppViewModel) {
    val ui by vm.ui.collectAsStateWithLifecycle()

    Column(Modifier.fillMaxSize()) {
        Row(
            Modifier
                .fillMaxWidth()
                .padding(start = 20.dp, end = 8.dp, top = 10.dp, bottom = 10.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Column(Modifier.weight(1f)) {
                Text(
                    "Verity",
                    color = MainText,
                    fontSize = 22.sp,
                    fontWeight = FontWeight.Bold,
                )
                Text(
                    if (ui.connected) "Connected" else "Offline",
                    color = if (ui.connected) DimText else ErrorRed,
                    fontSize = 13.sp,
                )
            }
            IconButton(onClick = { vm.openSettings() }) {
                Icon(Icons.Filled.Settings, contentDescription = "Settings", tint = DimText)
            }
        }
        HorizontalDivider(color = Hairline, thickness = 0.5.dp)

        LazyColumn(Modifier.fillMaxSize()) {
            item { SectionHeader("Topics") }
            if (ui.topics.isEmpty()) {
                item { EmptyHint(if (ui.connected) "No topics yet" else "Connecting to Telegram…") }
            }
            items(ui.topics, key = { "topic-${it.id}" }) { t ->
                Box(Modifier.animateItem()) {
                ConversationRow(
                    title = t.title,
                    subtitle = "",
                    dimmed = false,
                    onClick = {
                        vm.open(
                            Conversation(
                                kind = ConversationKind.TOPIC,
                                id = t.id,
                                title = t.title,
                            ),
                        )
                    },
                )
                }
            }

            item { SectionHeader("Direct") }
            if (ui.dms.isEmpty()) {
                item { EmptyHint(if (ui.connected) "No direct chats" else "—") }
            }
            items(ui.dms, key = { "dm-${it.userId}" }) { d ->
                Box(Modifier.animateItem()) {
                val subtitle = listOfNotNull(
                    d.label.ifBlank { null },
                    if (d.available) null else "unavailable",
                ).joinToString(" · ")
                ConversationRow(
                    title = d.name,
                    subtitle = subtitle,
                    dimmed = !d.available,
                    avatarPath = ui.avatars[d.userId],
                    onClick = {
                        vm.open(
                            Conversation(
                                kind = ConversationKind.DM,
                                id = d.userId,
                                title = d.name,
                                subtitle = d.label,
                                available = d.available,
                            ),
                        )
                    },
                )
                }
            }
        }
    }
}

@Composable
private fun SectionHeader(text: String) {
    Text(
        text.uppercase(),
        color = DimText,
        fontSize = 12.sp,
        fontWeight = FontWeight.SemiBold,
        letterSpacing = 1.5.sp,
        modifier = Modifier.padding(start = 20.dp, top = 18.dp, bottom = 6.dp),
    )
}

@Composable
private fun EmptyHint(text: String) {
    Text(
        text,
        color = DimText,
        fontSize = 14.sp,
        modifier = Modifier.padding(horizontal = 20.dp, vertical = 12.dp),
    )
}

@Composable
internal fun UserAvatar(
    path: String?,
    name: String,
    size: Dp,
    modifier: Modifier = Modifier,
) {
    val bmp by produceState<ImageBitmap?>(null, path) {
        value = if (path == null) {
            null
        } else {
            withContext(Dispatchers.Default) {
                runCatching { BitmapFactory.decodeFile(path)?.asImageBitmap() }
                    .getOrNull()
            }
        }
    }
    val b = bmp
    Box(
        modifier.size(size).clip(CircleShape).background(BubbleFill),
        contentAlignment = Alignment.Center,
    ) {
        if (b != null) {
            Image(
                b,
                contentDescription = name,
                modifier = Modifier.fillMaxSize(),
                contentScale = ContentScale.Crop,
            )
        } else {
            Text(
                name.firstOrNull()?.uppercase() ?: "?",
                color = DimText,
                fontSize = (size.value * 0.4f).sp,
                fontWeight = FontWeight.SemiBold,
            )
        }
    }
}

@Composable
private fun ConversationRow(
    title: String,
    subtitle: String,
    dimmed: Boolean,
    onClick: () -> Unit,
    avatarPath: String? = null,
) {
    Row(
        Modifier
            .fillMaxWidth()
            .clickable(onClick = onClick)
            .alpha(if (dimmed) 0.4f else 1f)
            .padding(start = 16.dp, end = 20.dp, top = 10.dp, bottom = 10.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        UserAvatar(avatarPath, title, 40.dp)
        Column(Modifier.padding(start = 12.dp)) {
            Text(
                title,
                color = MainText,
                fontSize = 16.sp,
                fontWeight = FontWeight.Medium,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            if (subtitle.isNotBlank()) {
                Text(
                    subtitle,
                    color = DimText,
                    fontSize = 13.sp,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }
    }
    HorizontalDivider(color = Hairline, thickness = 0.5.dp)
}
