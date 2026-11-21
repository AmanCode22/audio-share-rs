package io.audiosharers.client

import android.service.quicksettings.Tile
import android.service.quicksettings.TileService
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch

/**
 * Quick-settings tile: starts/stops playback using the last saved settings,
 * mirroring the original app's tile but with automatic reconnect behind it.
 */
class QsTileService : TileService() {

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    override fun onStartListening() {
        super.onStartListening()
        updateTile()
    }

    override fun onClick() {
        super.onClick()
        val tile = qsTile ?: return
        val playing = PlayerStateBus.state.value is PlayerState.Playing
        scope.launch {
            if (playing) {
                PlaybackService.stop(applicationContext)
            } else {
                val s = SettingsStore(applicationContext).current()
                PlaybackService.start(applicationContext, s)
            }
        }
        tile.state = if (playing) Tile.STATE_INACTIVE else Tile.STATE_ACTIVE
        tile.updateTile()
    }

    override fun onDestroy() {
        scope.cancel()
        super.onDestroy()
    }

    private fun updateTile() {
        val tile = qsTile ?: return
        val playing = PlayerStateBus.state.value is PlayerState.Playing
        tile.state = if (playing) Tile.STATE_ACTIVE else Tile.STATE_INACTIVE
        tile.subtitle = if (playing) getString(R.string.qs_active) else getString(R.string.qs_inactive)
        tile.updateTile()
    }
}
