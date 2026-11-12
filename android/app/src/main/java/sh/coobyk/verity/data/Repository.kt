package sh.coobyk.verity.data

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONObject
import java.util.concurrent.TimeUnit

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

class ApiException(val status: Int, message: String) : Exception(message)

/**
 * Thin wrapper over the `verity serve` HTTP JSON API.
 *
 * Contract:
 *   GET  /api/topics                        -> {"me": long, "topics": [{"id": long, "title": str}]}
 *   GET  /api/topics/{id}/messages?limit=N  -> {"messages": [ChatMessage]}
 *   GET  /api/dms                           -> {"dms": [{"user_id","name","label","available"}]}
 *   GET  /api/dms/{id}/messages?limit=N     -> {"messages": [ChatMessage]}
 *   POST /api/messages                      -> {"topic": long} xor {"dm": long}, plus "text"
 *   errors                                  -> {"error": str} with non-2xx
 *
 * Everything except /api/health carries `Authorization: Bearer <token>`.
 */
class Repository {

    private val client = OkHttpClient.Builder()
        .connectTimeout(6, TimeUnit.SECONDS)
        .readTimeout(15, TimeUnit.SECONDS)
        .build()

    @Volatile
    private var baseUrl: String = Settings.DEFAULT_BASE_URL

    @Volatile
    private var token: String = ""

    fun configure(baseUrl: String, token: String) {
        this.baseUrl = Settings.normalizeBaseUrl(baseUrl)
        this.token = token.trim()
    }

    suspend fun topics(): Pair<Long, List<Topic>> = withContext(Dispatchers.IO) {
        val obj = get("/api/topics")
        val arr = obj.getJSONArray("topics")
        val topics = (0 until arr.length()).map { i ->
            val t = arr.getJSONObject(i)
            Topic(id = t.getLong("id"), title = t.optString("title"))
        }
        obj.getLong("me") to topics
    }

    suspend fun dms(): List<DirectChat> = withContext(Dispatchers.IO) {
        val arr = get("/api/dms").getJSONArray("dms")
        (0 until arr.length()).map { i ->
            val d = arr.getJSONObject(i)
            DirectChat(
                userId = d.getLong("user_id"),
                name = d.optString("name"),
                label = d.optString("label"),
                available = d.optBoolean("available", true),
            )
        }
    }

    suspend fun messages(conv: Conversation, limit: Int = 50): List<ChatMessage> =
        withContext(Dispatchers.IO) {
            val path = when (conv.kind) {
                ConversationKind.TOPIC -> "/api/topics/${conv.id}/messages"
                ConversationKind.DM -> "/api/dms/${conv.id}/messages"
            } + "?limit=$limit"
            val arr = get(path).getJSONArray("messages")
            (0 until arr.length()).map { i ->
                val m = arr.getJSONObject(i)
                ChatMessage(
                    id = m.getLong("id"),
                    date = m.optLong("date"),
                    sender = m.optString("sender"),
                    senderId = if (m.isNull("sender_id")) null else m.getLong("sender_id"),
                    text = m.optString("text"),
                )
            }.sortedBy { it.id }
        }

    suspend fun send(conv: Conversation, text: String) = withContext(Dispatchers.IO) {
        val body = JSONObject()
            .put(
                if (conv.kind == ConversationKind.TOPIC) "topic" else "dm",
                conv.id,
            )
            .put("text", text)
        post("/api/messages", body)
        Unit
    }

    // -- HTTP plumbing ------------------------------------------------------

    private fun get(path: String, auth: Boolean = true): JSONObject {
        val req = Request.Builder()
            .url(baseUrl + path)
            .apply { if (auth) header("Authorization", "Bearer $token") }
            .build()
        return execute(req)
    }

    private fun post(path: String, body: JSONObject): JSONObject {
        val req = Request.Builder()
            .url(baseUrl + path)
            .header("Authorization", "Bearer $token")
            .post(body.toString().toRequestBody(JSON))
            .build()
        return execute(req)
    }

    private fun execute(req: Request): JSONObject {
        client.newCall(req).execute().use { resp ->
            val text = resp.body?.string().orEmpty()
            if (!resp.isSuccessful) {
                val err = runCatching { JSONObject(text).optString("error") }
                    .getOrNull()
                    ?.takeIf { it.isNotBlank() }
                throw ApiException(resp.code, err ?: "HTTP ${resp.code}")
            }
            return if (text.isBlank()) JSONObject() else JSONObject(text)
        }
    }

    companion object {
        private val JSON = "application/json; charset=utf-8".toMediaType()
    }
}
