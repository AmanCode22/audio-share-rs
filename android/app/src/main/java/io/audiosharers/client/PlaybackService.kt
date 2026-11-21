package io.audiosharers.client

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel

/**
 * Foreground service that owns the [NetClient] while audio is playing,
 * so playback survives the Activity being backgrounded.
 */
class PlaybackService : Service() {

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private var client: NetClient? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_STOP -> {
                stopSelf()
                return START_NOT_STICKY
            }
            else -> {
                val host = intent?.getStringExtra(EXTRA_HOST) ?: return START_NOT_STICKY
                val port = intent.getIntExtra(EXTRA_PORT, 65530)
                val tcpData = intent.getBooleanExtra(EXTRA_TCP, false)
                val bufferMs = intent.getIntExtra(EXTRA_BUFFER, 120)
                val volume = intent.getFloatExtra(EXTRA_VOLUME, 1f)
                val loudness = intent.getIntExtra(EXTRA_LOUDNESS, 0)

                startInForeground()
                client?.stop()
                client = NetClient(
                    host = host,
                    port = port,
                    tcpData = tcpData,
                    bufferMs = bufferMs,
                    volume = volume,
                    loudnessGainMb = loudness,
                ) { state ->
                    PlayerStateBus.publish(state)
                    if (state is PlayerState.Idle) stopSelf()
                }.also { it.start() }
            }
        }
        return START_STICKY
    }

    override fun onDestroy() {
        client?.stop()
        client = null
        scope.cancel()
        PlayerStateBus.publish(PlayerState.Idle)
        super.onDestroy()
    }

    private fun startInForeground() {
        val nm = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            nm.createNotificationChannel(
                NotificationChannel(
                    CHANNEL_ID,
                    getString(R.string.notif_channel_name),
                    NotificationManager.IMPORTANCE_LOW
                )
            )
        }
        val stopIntent = PendingIntent.getService(
            this, 1,
            Intent(this, PlaybackService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val contentIntent = PendingIntent.getActivity(
            this, 2,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val notification: Notification = NotificationCompat.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_stat_audio)
            .setContentTitle(getString(R.string.notif_title))
            .setContentText(getString(R.string.notif_text))
            .setContentIntent(contentIntent)
            .addAction(0, getString(R.string.notif_stop), stopIntent)
            .setOngoing(true)
            .build()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(NOTIF_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK)
        } else {
            startForeground(NOTIF_ID, notification)
        }
    }

    companion object {
        const val ACTION_STOP = "io.audiosharers.client.STOP"
        const val EXTRA_HOST = "host"
        const val EXTRA_PORT = "port"
        const val EXTRA_TCP = "tcp_data"
        const val EXTRA_BUFFER = "buffer_ms"
        const val EXTRA_VOLUME = "volume"
        const val EXTRA_LOUDNESS = "loudness_mb"
        const val CHANNEL_ID = "playback"
        const val NOTIF_ID = 42

        /** Convenience starter used by the Activity and the QS tile. */
        fun start(context: Context, s: Settings) {
            val intent = Intent(context, PlaybackService::class.java)
                .putExtra(EXTRA_HOST, s.host)
                .putExtra(EXTRA_PORT, s.port)
                .putExtra(EXTRA_TCP, s.tcpData)
                .putExtra(EXTRA_BUFFER, s.bufferMs)
                .putExtra(EXTRA_VOLUME, s.volume)
                .putExtra(EXTRA_LOUDNESS, s.loudnessMb)
            context.startForegroundService(intent)
        }

        fun stop(context: Context) {
            context.startService(
                Intent(context, PlaybackService::class.java).setAction(ACTION_STOP)
            )
        }
    }
}

/** Simple process-wide state bus between the service and the UI. */
object PlayerStateBus {
    private val flow = kotlinx.coroutines.flow.MutableStateFlow<PlayerState>(PlayerState.Idle)
    val state: kotlinx.coroutines.flow.StateFlow<PlayerState> = flow

    fun publish(state: PlayerState) {
        flow.value = state
    }
}
