package io.audiosharers.client

import android.content.Context
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.floatPreferencesKey
import androidx.datastore.preferences.core.intPreferencesKey
import androidx.datastore.preferences.core.stringPreferencesKey
import androidx.datastore.preferences.preferencesDataStore
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.map

private val Context.dataStore by preferencesDataStore(name = "audioshare")

/** Persisted connection + playback settings. */
data class Settings(
    val host: String,
    val port: Int,
    val tcpData: Boolean,
    val bufferMs: Int,
    val volume: Float,
    val loudnessMb: Int,
)

class SettingsStore(private val context: Context) {
    private object Keys {
        val HOST = stringPreferencesKey("host")
        val PORT = intPreferencesKey("port")
        val TCP = intPreferencesKey("tcp_data") // 0/1
        val BUFFER = intPreferencesKey("buffer_ms")
        val VOLUME = floatPreferencesKey("volume")
        val LOUDNESS = intPreferencesKey("loudness_mb")
    }

    val settings: Flow<Settings> = context.dataStore.data.map { p ->
        Settings(
            host = p[Keys.HOST] ?: "192.168.1.10",
            port = p[Keys.PORT] ?: 65530,
            tcpData = (p[Keys.TCP] ?: 0) == 1,
            bufferMs = p[Keys.BUFFER] ?: 120,
            volume = p[Keys.VOLUME] ?: 1.0f,
            loudnessMb = p[Keys.LOUDNESS] ?: 0,
        )
    }

    suspend fun current(): Settings = settings.first()

    suspend fun save(s: Settings) {
        context.dataStore.edit { p ->
            p[Keys.HOST] = s.host
            p[Keys.PORT] = s.port
            p[Keys.TCP] = if (s.tcpData) 1 else 0
            p[Keys.BUFFER] = s.bufferMs
            p[Keys.VOLUME] = s.volume
            p[Keys.LOUDNESS] = s.loudnessMb
        }
    }
}
