package sh.coobyk.verity.data

import android.content.Context
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.stringPreferencesKey
import androidx.datastore.preferences.preferencesDataStore
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.map

private val Context.dataStore by preferencesDataStore(name = "verity")

/** Persists the bridge base URL and bearer token in Preferences DataStore. */
class Settings(private val context: Context) {

    private object Keys {
        val BASE_URL = stringPreferencesKey("base_url")
        val TOKEN = stringPreferencesKey("token")
    }

    val baseUrl: Flow<String> =
        context.dataStore.data.map { it[Keys.BASE_URL] ?: DEFAULT_BASE_URL }

    val token: Flow<String> =
        context.dataStore.data.map { it[Keys.TOKEN] ?: "" }

    suspend fun setBaseUrl(value: String) {
        context.dataStore.edit { it[Keys.BASE_URL] = normalizeBaseUrl(value) }
    }

    suspend fun setToken(value: String) {
        context.dataStore.edit { it[Keys.TOKEN] = value.trim() }
    }

    companion object {
        const val DEFAULT_BASE_URL = "http://192.168.1.100:8787"

        fun normalizeBaseUrl(raw: String): String =
            raw.trim().trimEnd('/').ifBlank { DEFAULT_BASE_URL }
    }
}
