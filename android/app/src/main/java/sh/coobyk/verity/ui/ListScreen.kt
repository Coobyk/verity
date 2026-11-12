package sh.coobyk.verity.ui

import android.graphics.BitmapFactory
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
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
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
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
    val pinned by vm.pinned.collectAsStateWithLifecycle()

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
            items(
                // Stable sort keeps name order inside pinned/unpinned groups.
                ui.topics.sortedByDescending { "topic-${it.id}" in pinned },
                key = { "topic-${it.id}" },
            ) { t ->
                Box(Modifier.animateItem()) {
                val key = "topic-${t.id}"
                var menu by remember { mutableStateOf(false) }
                ConversationRow(
                    title = t.title,
                    subtitle = "",
                    dimmed = false,
                    pinned = key in pinned,
                    onLongClick = { menu = true },
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
                PinMenu(
                    expanded = menu,
                    pinned = key in pinned,
                    onDismiss = { menu = false },
                    onToggle = { vm.togglePin(key) },
                )
                }
            }

            item { SectionHeader("Direct") }
            if (ui.dms.isEmpty()) {
                item { EmptyHint(if (ui.connected) "No direct chats" else "—") }
            }
            items(
                ui.dms.sortedByDescending { "dm-${it.userId}" in pinned },
                key = { "dm-${it.userId}" },
            ) { d ->
                Box(Modifier.animateItem()) {
                val key = "dm-${d.userId}"
                var menu by remember { mutableStateOf(false) }
                val subtitle = listOfNotNull(
                    d.label.ifBlank { null },
                    if (d.available) null else "unavailable",
                ).joinToString(" · ")
                ConversationRow(
                    title = d.name,
                    subtitle = subtitle,
                    dimmed = !d.available,
                    pinned = key in pinned,
                    avatarPath = ui.avatars[d.userId],
                    onLongClick = { menu = true },
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
                PinMenu(
                    expanded = menu,
                    pinned = key in pinned,
                    onDismiss = { menu = false },
                    onToggle = { vm.togglePin(key) },
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

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun PinMenu(
    expanded: Boolean,
    pinned: Boolean,
    onDismiss: () -> Unit,
    onToggle: () -> Unit,
) {
    DropdownMenu(
        expanded = expanded,
        onDismissRequest = onDismiss,
        modifier = Modifier.background(ComposerFill),
    ) {
        DropdownMenuItem(
            text = {
                Text(
                    if (pinned) "Unpin from top" else "Pin to top",
                    color = MainText,
                )
            },
            onClick = {
                onToggle()
                onDismiss()
            },
        )
    }
}

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun ConversationRow(
    title: String,
    subtitle: String,
    dimmed: Boolean,
    onClick: () -> Unit,
    pinned: Boolean = false,
    avatarPath: String? = null,
    onLongClick: (() -> Unit)? = null,
) {
    Row(
        Modifier
            .fillMaxWidth()
            .combinedClickable(onClick = onClick, onLongClick = onLongClick)
            .alpha(if (dimmed) 0.4f else 1f)
            .padding(start = 16.dp, end = 20.dp, top = 10.dp, bottom = 10.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        UserAvatar(avatarPath, title, 40.dp)
        Column(Modifier.weight(1f).padding(start = 12.dp)) {
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
        if (pinned) {
            Text(
                "PINNED",
                color = DimText,
                fontSize = 10.sp,
                fontWeight = FontWeight.Medium,
            )
        }
    }
    HorizontalDivider(color = Hairline, thickness = 0.5.dp)
}
