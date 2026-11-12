package sh.coobyk.verity.data

import android.content.Context
import android.os.Build
import android.util.Log
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.suspendCancellableCoroutine
import org.drinkless.tdlib.TdApi
import org.drinkless.tdlib.Client
import sh.coobyk.verity.BuildConfig
import java.io.File
import java.util.Locale
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.coroutines.resume
import kotlin.coroutines.resumeWithException

/* ---------------------------------------------------------------------------
 * UI-facing models — identical to what the Compose screens already consume
 * (previously fed by the `verity serve` HTTP bridge; now fed by TDLib).
 * ------------------------------------------------------------------------- */

data class Topic(val id: Long, val title: String)

data class DirectChat(
    val userId: Long,
    val name: String,
    val label: String,
    val available: Boolean,
)

data class ChatMessage(
    val id: Long,
    val date: Long, // epoch seconds
    val sender: String,
    val senderId: Long?,
    val text: String,
)

enum class ConversationKind { TOPIC, DM }

data class Conversation(
    val kind: ConversationKind,
    val id: Long,
    val title: String,
    val subtitle: String = "",
    val available: Boolean = true,
)

class TdException(val code: Int, message: String) : Exception(message)

/**
 * Data layer over TDLib (`libtdjni` + the generated `TdApi` classes, vendored
 * under `org/drinkless`). Owns the single `Client`, the auth state machine and
 * caches of chats/users. All flows are safe to collect on the main thread —
 * TDLib delivers callbacks on its own handler thread and StateFlow marshals
 * the hand-off for free.
 */
class TdRepository(context: Context) {

    sealed interface AuthState {
        /** Client created, waiting for TDLib to ask for parameters/phone. */
        data object Starting : AuthState
        data object WaitPhone : AuthState
        data object WaitCode : AuthState
        data object WaitPassword : AuthState
        /** QR/link confirmation on another device. */
        data object WaitOtherDevice : AuthState
        data object Ready : AuthState
    }

    private val appContext = context.applicationContext
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)

    // -- Flows the ViewModel exposes -----------------------------------------

    val authState = MutableStateFlow<AuthState>(AuthState.Starting)
    /** Transient error during login (bad code etc). */
    val authError = MutableStateFlow<String?>(null)
    /** True while an auth request is in flight. */
    val authBusy = MutableStateFlow(false)

    /** TDLib connection to Telegram reached `ConnectionStateReady`. */
    val connected = MutableStateFlow(false)
    val me = MutableStateFlow<Long?>(null)

    val topics = MutableStateFlow<List<Topic>>(emptyList())
    val dms = MutableStateFlow<List<DirectChat>>(emptyList())

    /** Messages of the currently open conversation, oldest first. */
    val messages = MutableStateFlow<List<ChatMessage>>(emptyList())

    /** One-shot status string surfaced as a snackbar. */
    val status = MutableStateFlow<String?>(null)

    // -- TDLib state ----------------------------------------------------------

    @Volatile
    private var client: Client? = null

    /** chatId -> Chat (populated by UpdateNewChat + GetChat). */
    private val chats = ConcurrentHashMap<Long, TdApi.Chat>()

    /** userId -> User. */
    private val users = ConcurrentHashMap<Long, TdApi.User>()

    /** topic messageThreadId -> forum supergroup chatId. */
    private val topicChat = ConcurrentHashMap<Long, Long>()

    private class OpenConv(val chatId: Long, val threadId: Long)

    @Volatile
    private var openConv: OpenConv? = null

    private val refreshing = AtomicBoolean(false)

    @Volatile
    private var refreshQueued = false

    init {
        // The vendored Client.java loads `tdjni` itself in a static block; do it
        // here too so a missing .so degrades into an error instead of a crash.
        val libOk = runCatching { System.loadLibrary("tdjni") }
            .onFailure { Log.w(TAG, "libtdjni not loaded", it) }
            .isSuccess
        if (libOk) {
            runCatching { Client.execute(TdApi.SetLogVerbosityLevel(1)) }
            newClient()
        } else {
            authError.value = "TDLib native library (libtdjni) is missing"
        }
    }

    /* ---- Client + update dispatch ----------------------------------------- */

    private fun newClient() {
        client = Client.create(
            { obj -> onUpdate(obj) },
            { t -> Log.w(TAG, "update handler exception", t) },
            { t -> Log.w(TAG, "default exception handler", t) },
        )
    }

    private fun onUpdate(obj: TdApi.Object) {
        when (obj.constructor) {
            TdApi.UpdateAuthorizationState.CONSTRUCTOR ->
                onAuthState((obj as TdApi.UpdateAuthorizationState).authorizationState)

            TdApi.UpdateNewChat.CONSTRUCTOR -> {
                val chat = (obj as TdApi.UpdateNewChat).chat
                chats[chat.id] = chat
                scheduleRefresh()
            }

            TdApi.UpdateChatTitle.CONSTRUCTOR -> {
                val u = obj as TdApi.UpdateChatTitle
                chats[u.chatId]?.let { it.title = u.title }
                scheduleRefresh()
            }

            TdApi.UpdateChatPosition.CONSTRUCTOR -> {
                // A chat entered/left the main list.
                val u = obj as TdApi.UpdateChatPosition
                if (u.position.list.constructor == TdApi.ChatListMain.CONSTRUCTOR) {
                    scheduleRefresh()
                }
            }

            TdApi.UpdateChatLastMessage.CONSTRUCTOR -> scheduleRefresh()

            TdApi.UpdateUser.CONSTRUCTOR -> {
                val u = (obj as TdApi.UpdateUser).user
                users[u.id] = u
                scheduleRefresh()
            }

            TdApi.UpdateNewMessage.CONSTRUCTOR ->
                onNewMessage((obj as TdApi.UpdateNewMessage).message)

            TdApi.UpdateMessageContent.CONSTRUCTOR -> {
                val u = obj as TdApi.UpdateMessageContent
                val open = openConv
                if (open != null && u.chatId == open.chatId) {
                    val newText = contentText(u.newContent)
                    messages.update { list ->
                        list.map { if (it.id == u.messageId) it.copy(text = newText) else it }
                    }
                }
            }

            TdApi.UpdateDeleteMessages.CONSTRUCTOR -> {
                val u = obj as TdApi.UpdateDeleteMessages
                val open = openConv
                if (open != null && u.chatId == open.chatId && u.isPermanent) {
                    val gone = u.messageIds.toSet()
                    messages.update { list -> list.filterNot { it.id in gone } }
                }
            }

            TdApi.UpdateConnectionState.CONSTRUCTOR -> {
                val st = (obj as TdApi.UpdateConnectionState).state
                connected.value = st is TdApi.ConnectionStateReady
            }

            else -> {}
        }
    }

    /* ---- Auth state machine ------------------------------------------------ */

    private fun onAuthState(state: TdApi.AuthorizationState) {
        when (state.constructor) {
            TdApi.AuthorizationStateWaitTdlibParameters.CONSTRUCTOR -> sendParameters()
            TdApi.AuthorizationStateWaitPhoneNumber.CONSTRUCTOR ->
                setStep(AuthState.WaitPhone)
            TdApi.AuthorizationStateWaitCode.CONSTRUCTOR ->
                setStep(AuthState.WaitCode)
            TdApi.AuthorizationStateWaitPassword.CONSTRUCTOR ->
                setStep(AuthState.WaitPassword)
            TdApi.AuthorizationStateWaitOtherDeviceConfirmation.CONSTRUCTOR ->
                setStep(AuthState.WaitOtherDevice)
            TdApi.AuthorizationStateReady.CONSTRUCTOR -> scope.launch { onReady() }
            TdApi.AuthorizationStateLoggingOut.CONSTRUCTOR,
            TdApi.AuthorizationStateClosing.CONSTRUCTOR
            -> authState.value = AuthState.Starting
            TdApi.AuthorizationStateClosed.CONSTRUCTOR -> {
                // Session DB is wiped; a fresh client restarts the auth flow.
                authState.value = AuthState.Starting
                newClient()
            }
            else ->
                // WaitEmailAddress / WaitEmailCode / WaitRegistration and friends:
                // not part of the phone-code flow we support; tell the user.
                status.value = "Telegram asks for ${state.javaClass.simpleName} — " +
                    "finish logging in on another device"
        }
    }

    private fun setStep(step: AuthState) {
        authError.value = null
        authBusy.value = false
        authState.value = step
    }

    private fun sendParameters() {
        val dir = File(appContext.filesDir, "td").apply { mkdirs() }.absolutePath
        client?.send(
            TdApi.SetTdlibParameters(
                false, dir, dir, ByteArray(0),
                true, true, true, false,
                BuildConfig.TDLIB_API_ID, BuildConfig.TDLIB_API_HASH,
                Locale.getDefault().language.ifBlank { "en" },
                "${Build.MANUFACTURER} ${Build.MODEL}".trim(),
                "Android ${Build.VERSION.RELEASE}",
                BuildConfig.VERSION_NAME,
            ),
        ) { obj ->
            if (obj is TdApi.Error) {
                authError.value = "TDLib params: ${obj.message}"
            }
        }
    }

    private suspend fun onReady() {
        authState.value = AuthState.Ready
        authBusy.value = false
        runCatching { me.value = exec(TdApi.GetMe()).id }
        refreshChats()
    }

    fun sendPhone(phone: String) = authCall {
        exec(TdApi.SetAuthenticationPhoneNumber(phone.trim(), null))
    }

    fun sendCode(code: String) = authCall {
        exec(TdApi.CheckAuthenticationCode(code.trim()))
    }

    fun sendPassword(password: String) = authCall {
        exec(TdApi.CheckAuthenticationPassword(password))
    }

    fun logout() {
        scope.launch {
            runCatching { exec(TdApi.LogOut()) }
                .onFailure { status.value = it.message }
        }
    }

    private fun authCall(block: suspend () -> Unit) {
        if (authBusy.value) return
        authError.value = null
        authBusy.value = true
        scope.launch {
            try {
                block()
            } catch (e: Exception) {
                authError.value = e.message ?: e.javaClass.simpleName
            } finally {
                authBusy.value = false
            }
        }
    }

    /* ---- Chat listing ------------------------------------------------------ */

    fun refreshChats() {
        if (authState.value != AuthState.Ready) return
        if (!refreshing.compareAndSet(false, true)) return
        scope.launch {
            try {
                val ids = exec(TdApi.GetChats(TdApi.ChatListMain(), 200)).chatIds
                val dmRows = ArrayList<DirectChat>()
                val topicRows = ArrayList<Topic>()
                for (id in ids) {
                    val chat = chats[id]
                        ?: runCatching { exec(TdApi.GetChat(id)) }.getOrNull()
                            ?.also { chats[id] = it }
                        ?: continue
                    when (val type = chat.type) {
                        is TdApi.ChatTypePrivate -> {
                            val user = users[type.userId]
                                ?: runCatching { exec(TdApi.GetUser(type.userId)) }
                                    .getOrNull()?.also { users[it.id] = it }
                            if (user != null && user.type is TdApi.UserTypeBot) {
                                dmRows += DirectChat(
                                    userId = type.userId,
                                    name = chat.title.ifBlank { displayName(user) },
                                    label = usernameLabel(user),
                                    available = true,
                                )
                            }
                        }
                        is TdApi.ChatTypeSupergroup -> {
                            if (!type.isChannel) {
                                loadTopics(chat.id, topicRows)
                            }
                        }
                        else -> {}
                    }
                }
                dms.value = dmRows.sortedBy { it.name.lowercase() }
                topics.value = topicRows
            } catch (e: Exception) {
                status.value = e.message ?: "Can't load chats"
            } finally {
                refreshing.set(false)
            }
        }
    }

    private suspend fun loadTopics(chatId: Long, out: MutableList<Topic>) {
        // A non-forum group fails with an Error — swallowed by runCatching.
        val res = runCatching {
            exec(TdApi.GetForumTopics(chatId, "", 0, 0L, 0, 100))
        }.getOrNull() ?: return
        for (t in res.topics) {
            val info = t.info
            topicChat[info.forumTopicId.toLong()] = chatId
            out += Topic(id = info.forumTopicId.toLong(), title = info.name)
        }
    }

    /** Debounced refresh — coalesces bursts of position/title/user updates. */
    private fun scheduleRefresh() {
        if (authState.value != AuthState.Ready) return
        if (refreshQueued) return
        refreshQueued = true
        scope.launch {
            delay(1500)
            refreshQueued = false
            refreshChats()
        }
    }

    /* ---- Open conversation ------------------------------------------------- */

    fun openChat(conv: Conversation) {
        val open = when (conv.kind) {
            // For a private chat, chat.id == user.id.
            ConversationKind.DM -> OpenConv(conv.id, 0L)
            ConversationKind.TOPIC -> {
                val chatId = topicChat[conv.id]
                if (chatId == null) {
                    status.value = "Can't find that topic's group — refresh the list"
                    refreshChats()
                    return
                }
                OpenConv(chatId, conv.id)
            }
        }
        openConv = open
        messages.value = emptyList()
        scope.launch { loadHistory(open) }
    }

    fun closeChat() {
        openConv = null
        messages.value = emptyList()
    }

    private suspend fun loadHistory(open: OpenConv) {
        val arr = try {
            if (open.threadId != 0L) {
                // A forum topic IS a message thread — its id is the
                // topic-create service message's id.
                exec(
                    TdApi.GetMessageThreadHistory(
                        open.chatId, open.threadId, 0L, 0, 100,
                    ),
                ).messages
            } else {
                exec(TdApi.GetChatHistory(open.chatId, 0L, 0, 50, false)).messages
            }
        } catch (e: Exception) {
            if (openConv === open) status.value = e.message ?: "Can't load history"
            return
        }
        val rows = (arr ?: emptyArray())
            .map { toChatMessage(it) }
            .sortedBy { it.id }
        if (openConv === open) {
            messages.value = rows
        }
    }

    private fun onNewMessage(m: TdApi.Message) {
        val open = openConv
        val inThread =
            (m.topicId as? TdApi.MessageTopicForum)?.forumTopicId?.toLong()
        if (open != null && m.chatId == open.chatId &&
            (open.threadId == 0L || inThread == open.threadId)
        ) {
            scope.launch {
                val row = toChatMessage(m)
                messages.update { list ->
                    if (list.any { it.id == row.id }) list
                    else (list + row).sortedBy(sortKey)
                }
            }
        } else {
            // Activity in a chat we haven't listed yet.
            scheduleRefresh()
        }
    }

    /* ---- Sending ----------------------------------------------------------- */

    /**
     * Sends [text] into the open conversation. [optimistic] is appended first;
     * on success it's replaced by the real message (UpdateNewMessage for our
     * own message may arrive first — deduped by id either way).
     */
    suspend fun send(text: String, optimistic: ChatMessage) {
        val open = openConv ?: return
        messages.update { it + optimistic }
        val content = TdApi.InputMessageText(TdApi.FormattedText(text, null), null, false)
        val sent = try {
            exec(TdSend.message(open.chatId, open.threadId, content))
        } catch (e: Exception) {
            messages.update { list -> list.filterNot { it.id == optimistic.id } }
            status.value = e.message ?: "Send failed"
            return
        }
        val row = toChatMessage(sent)
        messages.update { list ->
            (list.filterNot { it.id == optimistic.id || it.id == row.id } + row)
                .sortedBy(sortKey)
        }
    }

    /** Single home for the SendMessage call — topic routing lives here. */
    private object TdSend {
        fun message(
            chatId: Long,
            threadId: Long,
            content: TdApi.InputMessageContent,
        ): TdApi.SendMessage =
            TdApi.SendMessage(
                chatId,
                if (threadId == 0L) null else TdApi.MessageTopicForum(threadId.toInt()),
                null, null, null, content,
            )
    }

    /* ---- Request plumbing -------------------------------------------------- */

    private suspend fun <T : TdApi.Object> exec(fn: TdApi.Function<T>): T =
        suspendCancellableCoroutine { cont ->
            val c = client
            if (c == null) {
                cont.resumeWithException(TdException(0, "TDLib not ready"))
            } else {
                c.send(fn) { obj ->
                    if (obj is TdApi.Error) {
                        cont.resumeWithException(TdException(obj.code, obj.message))
                    } else {
                        @Suppress("UNCHECKED_CAST")
                        cont.resume(obj as T)
                    }
                }
            }
        }

    private suspend fun toChatMessage(m: TdApi.Message): ChatMessage {
        var senderId: Long? = null
        var sender = "?"
        when (val s = m.senderId) {
            is TdApi.MessageSenderUser -> {
                senderId = s.userId
                val u = users[s.userId]
                    ?: runCatching { exec(TdApi.GetUser(s.userId)) }
                        .getOrNull()?.also { users[it.id] = it }
                sender = u?.let { displayName(it) } ?: "user ${s.userId}"
            }
            is TdApi.MessageSenderChat -> {
                val c = chats[s.chatId]
                    ?: runCatching { exec(TdApi.GetChat(s.chatId)) }
                        .getOrNull()?.also { chats[it.id] = it }
                sender = c?.title ?: "chat ${s.chatId}"
            }
        }
        return ChatMessage(
            id = m.id,
            date = m.date.toLong(),
            sender = sender,
            senderId = senderId,
            text = contentText(m.content),
        )
    }

    private fun contentText(c: TdApi.MessageContent): String = when (c) {
        is TdApi.MessageText -> c.text.text
        is TdApi.MessagePhoto -> c.caption.text.ifBlank { "[photo]" }
        is TdApi.MessageVideo -> c.caption.text.ifBlank { "[video]" }
        is TdApi.MessageDocument -> c.caption.text.ifBlank {
            c.document.fileName.ifBlank { "[file]" }
        }
        is TdApi.MessageVoiceNote -> c.caption.text.ifBlank { "[voice]" }
        is TdApi.MessageSticker -> "[sticker ${c.sticker.emoji}]"
        is TdApi.MessageAnimatedEmoji -> c.emoji
        is TdApi.MessagePoll -> "[poll]"
        else -> "[${c.javaClass.simpleName.removePrefix("Message")}]"
    }

    private fun displayName(u: TdApi.User): String =
        listOf(u.firstName, u.lastName)
            .filter { it.isNotBlank() }
            .joinToString(" ")
            .ifBlank { "user ${u.id}" }

    private fun usernameLabel(u: TdApi.User): String {
        // INTEGRATION: `User.usernames` exists in TdApi 1.8.x. For older API
        // use `u.username` instead.
        val handle = u.usernames?.activeUsernames?.firstOrNull()
        return if (handle.isNullOrBlank()) "bot" else "@$handle"
    }

    fun clearStatus() {
        status.value = null
    }

    private companion object {
        const val TAG = "TdRepository"

        /** Optimistic rows have negative ids — keep them pinned to the end. */
        val sortKey: (ChatMessage) -> Long = { if (it.id < 0) Long.MAX_VALUE else it.id }
    }
}
