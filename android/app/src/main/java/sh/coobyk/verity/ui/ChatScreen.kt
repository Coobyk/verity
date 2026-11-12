package sh.coobyk.verity.ui

import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.util.LruCache
import androidx.activity.compose.PredictiveBackHandler
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.Image
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.MutableTransitionState
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInVertically
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.filled.Send
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextField
import androidx.compose.material3.TextFieldDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.material3.LocalTextStyle
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.text.withStyle
import org.drinkless.tdlib.TdApi
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.TextUnit
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import sh.coobyk.verity.data.ChatMessage
import sh.coobyk.verity.data.Conversation
import sh.coobyk.verity.data.ConversationKind
import sh.coobyk.verity.data.MsgBlock
import sh.coobyk.verity.math.TypstBridge
import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter

@OptIn(ExperimentalFoundationApi::class)
@Composable
fun ChatScreen(vm: AppViewModel, conv: Conversation) {
    val ui by vm.ui.collectAsStateWithLifecycle()
    val msgs by vm.messages.collectAsStateWithLifecycle()
    val sending by vm.sending.collectAsStateWithLifecycle()
    val me = ui.me

    // Streams gesture progress so App.kt can seek the nav transition;
    // completes → vm.back(), cancels → progress resets to -1f.
    PredictiveBackHandler { progress ->
        try {
            progress.collect { vm.backProgress.value = it.progress }
            vm.back()
        } finally {
            vm.backProgress.value = -1f
        }
    }

    // Centered max-width column like the desktop client.
    Box(Modifier.fillMaxSize(), contentAlignment = Alignment.TopCenter) {
        Column(Modifier.widthIn(max = 680.dp).fillMaxSize()) {
            // Header
            Row(
                Modifier
                    .fillMaxWidth()
                    .padding(start = 4.dp, end = 8.dp, top = 4.dp, bottom = 4.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                IconButton(onClick = { vm.back() }) {
                    Icon(
                        Icons.AutoMirrored.Filled.ArrowBack,
                        contentDescription = "Back",
                        tint = MainText,
                    )
                }
                Column(Modifier.weight(1f)) {
                    Text(
                        conv.title,
                        color = MainText,
                        fontSize = 17.sp,
                        fontWeight = FontWeight.SemiBold,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                    Text(
                        when {
                            conv.kind == ConversationKind.DM && !conv.available -> "Unavailable"
                            conv.subtitle.isNotBlank() -> conv.subtitle
                            else -> if (ui.connected) "Online" else "Offline"
                        },
                        color = DimText,
                        fontSize = 13.sp,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
            }
            HorizontalDivider(color = Hairline, thickness = 0.5.dp)

            // Messages
            val listState = rememberLazyListState()
            var prevSize by remember { mutableIntStateOf(0) }
            LaunchedEffect(msgs.size) {
                val wasEmpty = prevSize == 0
                prevSize = msgs.size
                if (msgs.isEmpty()) return@LaunchedEffect
                val lastVisible =
                    listState.layoutInfo.visibleItemsInfo.lastOrNull()?.index ?: 0
                when {
                    wasEmpty -> listState.scrollToItem(msgs.size - 1)
                    lastVisible >= msgs.size - 3 ->
                        listState.animateScrollToItem(msgs.size - 1)
                }
            }
            // Baseline = newest id once history loads; only messages
            // newer than that (live arrivals, optimistic sends) get the
            // entrance animation — rows scrolled into view from history
            // render instantly, keeping scrolling cheap.
            var baseId by remember { mutableLongStateOf(Long.MIN_VALUE) }
            LaunchedEffect(msgs.isNotEmpty()) {
                if (baseId == Long.MIN_VALUE && msgs.isNotEmpty()) {
                    baseId = msgs.last().id
                }
            }
            val played = remember { HashSet<Long>() }
            LazyColumn(
                state = listState,
                modifier = Modifier.weight(1f).fillMaxWidth(),
                contentPadding = PaddingValues(horizontal = 16.dp, vertical = 12.dp),
            ) {
                items(msgs, key = { it.id }, contentType = { "msg" }) { m ->
                    val enter = m.id > baseId && played.add(m.id)
                    val row: @Composable () -> Unit = {
                        MessageRow(m, mine = me != null && m.senderId == me)
                    }
                    if (enter) {
                        val appear = remember {
                            MutableTransitionState(false).apply { targetState = true }
                        }
                        AnimatedVisibility(
                            appear,
                            modifier = Modifier.animateItem(
                                fadeInSpec = null,
                                fadeOutSpec = null,
                            ),
                            enter =
                                fadeIn(tween(220)) +
                                    slideInVertically(tween(220)) { it / 4 },
                            exit = fadeOut(),
                        ) {
                            row()
                        }
                    } else {
                        Box(
                            Modifier.animateItem(
                                fadeInSpec = null,
                                fadeOutSpec = null,
                            ),
                        ) {
                            row()
                        }
                    }
                }
            }

            Composer(
                enabled = conv.kind == ConversationKind.TOPIC || conv.available,
                sending = sending,
                onSend = { vm.send(it) },
            )
        }
    }
}

@Composable
private fun MessageRow(m: ChatMessage, mine: Boolean) {
    if (mine) {
        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.End) {
            Box(
                Modifier
                    .padding(start = 56.dp, top = 2.dp, bottom = 2.dp)
                    .clip(RoundedCornerShape(20.dp))
                    .background(BubbleFill)
                    .padding(horizontal = 14.dp, vertical = 9.dp),
            ) {
                Column { m.blocks.forEach { MessageBlock(it, 20.sp) } }
            }
        }
    } else {
        Column(Modifier.fillMaxWidth().padding(top = 6.dp, bottom = 6.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    m.sender,
                    color = DimText,
                    fontSize = 12.sp,
                    fontWeight = FontWeight.Medium,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Spacer(Modifier.width(6.dp))
                Text(formatTime(m.date), color = DimText, fontSize = 12.sp)
            }
            m.blocks.forEach {
                MessageBlock(it, 21.sp, Modifier.padding(top = 2.dp))
            }
        }
    }
}

@Composable
private fun MessageBlock(
    b: MsgBlock,
    lineHeight: TextUnit,
    modifier: Modifier = Modifier,
) {
    when (b) {
        is MsgBlock.Text -> Text(
            remember(b.ft) { styled(b.ft) },
            color = MainText,
            fontSize = 15.sp,
            lineHeight = lineHeight,
            modifier = modifier,
        )
        is MsgBlock.Math -> MathView(b.tex, b.display, lineHeight)
    }
}

private object MathBitmaps {
    val cache = LruCache<String, Bitmap>(64)
}

@Composable
private fun MathView(tex: String, display: Boolean, lineHeight: TextUnit) {
    val density = LocalDensity.current
    val key = "${if (display) "d" else "i"}:$tex"
    val bmp by produceState<Bitmap?>(MathBitmaps.cache.get(key), tex, display, density) {
        if (value == null) {
            value = withContext(Dispatchers.Default) {
                runCatching {
                    TypstBridge.render(tex, display, density.density)
                        ?.let { BitmapFactory.decodeByteArray(it, 0, it.size) }
                }.getOrNull()?.also { MathBitmaps.cache.put(key, it) }
            }
        }
    }
    val b = bmp
    if (b == null) {
        Text(
            buildAnnotatedString {
                withStyle(SpanStyle(fontFamily = FontFamily.Monospace)) {
                    append(tex)
                }
            },
            color = MainText,
            fontSize = 13.5.sp,
        )
    } else {
        val img = remember(b) { b.asImageBitmap() }
        if (display) {
            Box(
                Modifier.fillMaxWidth().padding(vertical = 4.dp),
                contentAlignment = Alignment.Center,
            ) {
                Image(
                    img,
                    contentDescription = tex,
                    modifier = Modifier.widthIn(max = 560.dp),
                )
            }
        } else {
            Image(
                img,
                contentDescription = tex,
                modifier = Modifier.height(with(density) { (lineHeight * 1.4f).toDp() }),
            )
        }
    }
}

@Composable
private fun Composer(
    enabled: Boolean,
    sending: Boolean,
    onSend: (String) -> Unit,
) {
    var text by remember { mutableStateOf("") }

    fun submit() {
        if (enabled && !sending && text.isNotBlank()) {
            onSend(text)
            text = ""
        }
    }

    Surface(
        modifier = Modifier
            .fillMaxWidth()
            .imePadding()
            .padding(horizontal = 12.dp, vertical = 10.dp),
        shape = RoundedCornerShape(28.dp),
        color = ComposerFill,
        border = BorderStroke(0.5.dp, Hairline),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            TextField(
                value = text,
                onValueChange = { text = it },
                modifier = Modifier.weight(1f),
                enabled = enabled,
                placeholder = {
                    Text(
                        if (enabled) "Message…" else "Unavailable",
                        color = DimText,
                        fontSize = 15.sp,
                    )
                },
                maxLines = 4,
                textStyle = LocalTextStyle.current.copy(
                    color = MainText,
                    fontSize = 15.sp,
                ),
                colors = TextFieldDefaults.colors(
                    focusedTextColor = MainText,
                    unfocusedTextColor = MainText,
                    disabledTextColor = DimText,
                    focusedContainerColor = Color.Transparent,
                    unfocusedContainerColor = Color.Transparent,
                    disabledContainerColor = Color.Transparent,
                    cursorColor = MainText,
                    focusedIndicatorColor = Color.Transparent,
                    unfocusedIndicatorColor = Color.Transparent,
                    disabledIndicatorColor = Color.Transparent,
                ),
                keyboardOptions = KeyboardOptions(imeAction = ImeAction.Send),
                keyboardActions = KeyboardActions(onSend = { submit() }),
            )
            IconButton(
                onClick = { submit() },
                enabled = enabled && text.isNotBlank() && !sending,
            ) {
                Icon(
                    Icons.AutoMirrored.Filled.Send,
                    contentDescription = "Send",
                    tint = if (enabled && text.isNotBlank()) MainText else DimText,
                )
            }
        }
    }
}

private fun formatTime(epochSec: Long): String = try {
    Instant.ofEpochSecond(epochSec)
        .atZone(ZoneId.systemDefault())
        .format(DateTimeFormatter.ofPattern("HH:mm"))
} catch (e: Exception) {
    ""
}

/** TDLib FormattedText -> AnnotatedString with real span styling. */
private fun styled(ft: TdApi.FormattedText): AnnotatedString =
    buildAnnotatedString {
        append(ft.text)
        ft.entities?.forEach { e ->
            val style = when (e.type) {
                is TdApi.TextEntityTypeBold ->
                    SpanStyle(fontWeight = FontWeight.Bold)
                is TdApi.TextEntityTypeItalic ->
                    SpanStyle(fontStyle = FontStyle.Italic)
                is TdApi.TextEntityTypeUnderline ->
                    SpanStyle(textDecoration = TextDecoration.Underline)
                is TdApi.TextEntityTypeStrikethrough ->
                    SpanStyle(textDecoration = TextDecoration.LineThrough)
                is TdApi.TextEntityTypeCode, is TdApi.TextEntityTypePre ->
                    SpanStyle(
                        fontFamily = FontFamily.Monospace,
                        fontSize = 13.5.sp,
                        background = Color(0xFF16181C),
                    )
                is TdApi.TextEntityTypeTextUrl ->
                    SpanStyle(
                        color = Color(0xFF1D9BF0),
                        textDecoration = TextDecoration.Underline,
                    )
                is TdApi.TextEntityTypeSpoiler ->
                    SpanStyle(background = BubbleFill)
                else -> null
            }
            if (style != null) {
                val s = e.offset.coerceIn(0, ft.text.length)
                val end = (e.offset + e.length).coerceIn(0, ft.text.length)
                if (end > s) addStyle(style, s, end)
            }
        }
    }
