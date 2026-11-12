package sh.coobyk.verity.ui

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import org.drinkless.tdlib.TdApi
import sh.coobyk.verity.data.ChatMessage
import sh.coobyk.verity.data.Conversation
import sh.coobyk.verity.data.DirectChat
import sh.coobyk.verity.data.MsgBlock
import sh.coobyk.verity.data.TdRepository
import sh.coobyk.verity.data.Topic

/**
 * Thin adapter between [TdRepository] and the screens. No polling — TDLib
 * pushes updates (`UpdateNewMessage`, `UpdateNewChat`, …) into the repo's
 * StateFlows, which we simply mirror into the UI state.
 */
class AppViewModel(app: Application) : AndroidViewModel(app) {

    data class UiState(
        val me: Long? = null,
        val topics: List<Topic> = emptyList(),
        val dms: List<DirectChat> = emptyList(),
        val connected: Boolean = false,
        /** One-shot message consumed by the snackbar; null when idle. */
        val status: String? = null,
    )

    private val repo = TdRepository(app)

    val ui = MutableStateFlow(UiState())
    val conversation = MutableStateFlow<Conversation?>(null)
    val messages = MutableStateFlow<List<ChatMessage>>(emptyList())
    val showSettings = MutableStateFlow(false)
    val sending = MutableStateFlow(false)

    /**
     * Predictive-back progress, 0f..1f while a back gesture drags toward
     * the main list; -1f when idle/cancelled. Set by PredictiveBackHandler
     * in Chat/Settings screens, consumed by App.kt's nav transition.
     */
    val backProgress = MutableStateFlow(-1f)

    // Auth flows, consumed by LoginScreen / App.kt's screen gate.
    val authState = repo.authState
    val authError = repo.authError
    val authBusy = repo.authBusy

    private var optimisticId = 0L

    init {
        viewModelScope.launch { repo.me.collect { v -> ui.update { it.copy(me = v) } } }
        viewModelScope.launch { repo.topics.collect { v -> ui.update { it.copy(topics = v) } } }
        viewModelScope.launch { repo.dms.collect { v -> ui.update { it.copy(dms = v) } } }
        viewModelScope.launch {
            repo.connected.collect { v -> ui.update { it.copy(connected = v) } }
        }
        viewModelScope.launch {
            repo.status.collect { v ->
                if (v != null) {
                    ui.update { it.copy(status = v) }
                    repo.clearStatus()
                }
            }
        }
        // Repo already scopes `messages` to the open conversation.
        viewModelScope.launch { repo.messages.collect { v -> messages.value = v } }
    }

    /** Called via LifecycleResumeEffect; refreshes lists on resume. */
    fun setForeground(fg: Boolean) {
        if (fg) repo.refreshChats()
    }

    fun open(conv: Conversation) {
        conversation.value = conv
        repo.openChat(conv)
    }

    fun back() {
        conversation.value = null
        repo.closeChat()
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

    fun refresh() {
        repo.refreshChats()
    }

    // -- Login ---------------------------------------------------------------

    fun sendPhone(phone: String) = repo.sendPhone(phone)
    fun sendCode(code: String) = repo.sendCode(code)
    fun sendPassword(password: String) = repo.sendPassword(password)

    fun logout() {
        repo.logout()
        showSettings.value = false
        conversation.value = null
    }

    // -- Messaging -------------------------------------------------------------

    fun send(text: String) {
        if (conversation.value == null) return
        val trimmed = text.trim()
        if (trimmed.isEmpty()) return

        val optimistic = ChatMessage(
            id = --optimisticId,
            date = System.currentTimeMillis() / 1000,
            sender = "You",
            senderId = ui.value.me,
            blocks = listOf(MsgBlock.Text(TdApi.FormattedText(trimmed, null))),
        )

        viewModelScope.launch {
            sending.value = true
            try {
                repo.send(trimmed, optimistic)
            } finally {
                sending.value = false
            }
        }
    }
}
