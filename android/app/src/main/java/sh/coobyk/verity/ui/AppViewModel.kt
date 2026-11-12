package sh.coobyk.verity.ui

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import sh.coobyk.verity.data.ApiException
import sh.coobyk.verity.data.ChatMessage
import sh.coobyk.verity.data.Conversation
import sh.coobyk.verity.data.DirectChat
import sh.coobyk.verity.data.Repository
import sh.coobyk.verity.data.Settings
import sh.coobyk.verity.data.Topic

class AppViewModel(app: Application) : AndroidViewModel(app) {

    data class UiState(
        val me: Long? = null,
        val topics: List<Topic> = emptyList(),
        val dms: List<DirectChat> = emptyList(),
        val connected: Boolean = false,
        val baseUrl: String = Settings.DEFAULT_BASE_URL,
        val token: String = "",
        /** One-shot message consumed by the snackbar; null when idle. */
        val status: String? = null,
    )

    private val settings = Settings(app)
    private val repo = Repository()

    val ui = MutableStateFlow(UiState())
    val conversation = MutableStateFlow<Conversation?>(null)
    val messages = MutableStateFlow<List<ChatMessage>>(emptyList())
    val showSettings = MutableStateFlow(false)
    val sending = MutableStateFlow(false)

    /** Set from the UI via LifecycleResumeEffect; gates all polling. */
    private val foreground = MutableStateFlow(false)

    private var offlineNotified = false
    private var msgPollNotified = false
    private var lastLoadedConv: Conversation? = null
    private var optimisticId = 0L

    init {
        // Re-configure the client whenever settings land or change.
        viewModelScope.launch {
            combine(settings.baseUrl, settings.token, ::Pair).collect { (url, tok) ->
                repo.configure(url, tok)
                ui.update { it.copy(baseUrl = url, token = tok) }
                refreshLists()
            }
        }

        // Conversation lists: every ~30s while the app is in the foreground.
        viewModelScope.launch {
            foreground.collectLatest { fg ->
                while (fg && currentCoroutineContext().isActive) {
                    refreshLists()
                    delay(LIST_POLL_MS)
                }
            }
        }

        // Open conversation: every ~3s while in the foreground.
        viewModelScope.launch {
            combine(foreground, conversation, ::Pair).collectLatest { (fg, conv) ->
                if (!fg) return@collectLatest
                if (conv == null) {
                    messages.value = emptyList()
                    lastLoadedConv = null
                    return@collectLatest
                }
                if (conv != lastLoadedConv) {
                    messages.value = emptyList()
                    lastLoadedConv = conv
                    msgPollNotified = false
                }
                while (currentCoroutineContext().isActive) {
                    loadMessages(conv)
                    delay(MSG_POLL_MS)
                }
            }
        }
    }

    fun setForeground(fg: Boolean) {
        foreground.value = fg
    }

    fun open(conv: Conversation) {
        conversation.value = conv
    }

    fun back() {
        conversation.value = null
    }

    fun openSettings() {
        showSettings.value = true
    }

    fun closeSettings() {
        showSettings.value = false
    }

    fun clearStatus() {
        ui.update { it.copy(status = null) }
    }

    fun saveSettings(url: String, token: String) {
        viewModelScope.launch {
            settings.setBaseUrl(url)
            settings.setToken(token)
            showSettings.value = false
            // The settings flow re-emits and triggers refreshLists().
        }
    }

    fun refresh() {
        viewModelScope.launch { refreshLists() }
    }

    fun send(text: String) {
        val conv = conversation.value ?: return
        val trimmed = text.trim()
        if (trimmed.isEmpty()) return

        // Optimistic append; replaced by the refetch below or the next poll.
        messages.update {
            it + ChatMessage(
                id = --optimisticId,
                date = System.currentTimeMillis() / 1000,
                sender = "You",
                senderId = ui.value.me,
                text = trimmed,
            )
        }

        viewModelScope.launch {
            sending.value = true
            try {
                repo.send(conv, trimmed)
                loadMessages(conv)
            } catch (e: Exception) {
                setStatus(describe(e))
            } finally {
                sending.value = false
            }
        }
    }

    private suspend fun refreshLists() {
        try {
            val (me, topics) = repo.topics()
            val dms = repo.dms()
            offlineNotified = false
            ui.update { it.copy(connected = true, me = me, topics = topics, dms = dms) }
        } catch (e: Exception) {
            ui.update { it.copy(connected = false) }
            if (!offlineNotified) {
                offlineNotified = true
                setStatus(describe(e))
            }
        }
    }

    private suspend fun loadMessages(conv: Conversation) {
        try {
            messages.value = repo.messages(conv)
            msgPollNotified = false
        } catch (e: Exception) {
            if (!msgPollNotified) {
                msgPollNotified = true
                setStatus(describe(e))
            }
        }
    }

    private fun setStatus(msg: String) {
        ui.update { it.copy(status = msg) }
    }

    private fun describe(e: Throwable): String = when (e) {
        is ApiException -> e.message ?: "HTTP ${e.status}"
        is java.net.UnknownHostException,
        is java.net.ConnectException,
        -> "Can't reach bridge at ${ui.value.baseUrl}"
        is java.net.SocketTimeoutException -> "Bridge timed out"
        is org.json.JSONException -> "Bad response from bridge"
        is java.io.IOException -> "Network error"
        is IllegalArgumentException -> "Bad base URL — check settings"
        else -> e.message ?: e.javaClass.simpleName
    }

    private companion object {
        const val LIST_POLL_MS = 30_000L
        const val MSG_POLL_MS = 3_000L
    }
}
