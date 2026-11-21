package io.audiosharers.client

import android.Manifest
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularWavyProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3ExpressiveApi
import androidx.compose.material3.FilledIconButton
import androidx.compose.material3.IconButtonDefaults
import androidx.compose.material3.MaterialExpressiveTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.MotionScheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Slider
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.compose.ui.platform.LocalContext
import kotlinx.coroutines.launch

/**
 * Material 3 Expressive UI: connect to a laptop running `ashr serve`,
 * tune latency/volume, and monitor the stream live.
 */
class MainActivity : ComponentActivity() {

    private val notifPermission =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (Build.VERSION.SDK_INT >= 33) {
            notifPermission.launch(Manifest.permission.POST_NOTIFICATIONS)
        }
        val store = SettingsStore(applicationContext)
        setContent {
            MaterialExpressiveTheme(
                motionScheme = MotionScheme.expressive(),
                colorScheme = MaterialTheme.colorScheme,
            ) {
                HomeScreen(store)
            }
        }
    }
}

@OptIn(ExperimentalMaterial3ExpressiveApi::class)
@Composable
fun HomeScreen(store: SettingsStore) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val state by PlayerStateBus.state.collectAsState()

    var host by remember { mutableStateOf("192.168.1.10") }
    var port by remember { mutableStateOf("65530") }
    var tcpData by remember { mutableStateOf(false) }
    var bufferMs by remember { mutableStateOf(120f) }
    var volume by remember { mutableStateOf(1f) }
    var loudness by remember { mutableStateOf(0f) }
    var scanning by remember { mutableStateOf(false) }
    var scanResults by remember { mutableStateOf(listOf<Discovery.Server>()) }

    LaunchedEffect(Unit) {
        val s = store.current()
        host = s.host
        port = s.port.toString()
        tcpData = s.tcpData
        bufferMs = s.bufferMs.toFloat()
        volume = s.volume
        loudness = s.loudnessMb.toFloat()
    }

    fun settings() = Settings(
        host = host.trim(),
        port = port.toIntOrNull() ?: 65530,
        tcpData = tcpData,
        bufferMs = bufferMs.toInt(),
        volume = volume,
        loudnessMb = loudness.toInt(),
    )

    Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(20.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        Text("Audio Share", style = MaterialTheme.typography.displaySmall)
        Text(
            "Your phone becomes the speaker of your computer.",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )

        // ---- connection card ----
        Card(
            modifier = Modifier.fillMaxWidth(),
            colors = CardDefaults.cardColors(),
        ) {
            Column(
                modifier = Modifier.padding(16.dp),
                verticalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically) {
                    FilledIconButton(
                        onClick = {
                            scope.launch {
                                scanning = true
                                scanResults = Discovery.browse(context, 3000)
                                scanResults.firstOrNull()?.let {
                                    host = it.host
                                    port = it.port.toString()
                                }
                                scanning = false
                            }
                        },
                        shapes = IconButtonDefaults.shapes(),
                    ) { Text(if (scanning) "…" else "Scan") }
                    Text("find servers on this Wi-Fi", style = MaterialTheme.typography.bodySmall)
                }

                OutlinedTextField(
                    value = host,
                    onValueChange = { host = it },
                    label = { Text("Server address") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                OutlinedTextField(
                    value = port,
                    onValueChange = { port = it.filter { c -> c.isDigit() } },
                    label = { Text("Port") },
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )

                if (scanResults.isNotEmpty()) {
                    Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                        scanResults.forEach { srv ->
                            Card(modifier = Modifier.fillMaxWidth(), onClick = {
                                host = srv.host
                                port = srv.port.toString()
                            }) {
                                Text("${srv.name} — ${srv.host}:${srv.port}", modifier = Modifier.padding(12.dp))
                            }
                        }
                    }
                }

                Row(verticalAlignment = Alignment.CenterVertically) {
                    Switch(checked = tcpData, onCheckedChange = { tcpData = it })
                    Spacer(Modifier.width(10.dp))
                    Text("TCP audio (no UDP)", style = MaterialTheme.typography.bodyMedium)
                }

                LabeledSlider("Buffer", bufferMs, 40f, 400f, "%.0f ms") { bufferMs = it }
                LabeledSlider("Volume", volume, 0f, 1f, "%.0f%%", display = { (it * 100).toInt() }) { volume = it }
                LabeledSlider("Loudness boost", loudness, 0f, 3000f, "%.0f mB") { loudness = it }
            }
        }

        // ---- play button ----
        val playing = state is PlayerState.Playing || state is PlayerState.Connecting
        FilledIconButton(
            onClick = {
                val s = settings()
                scope.launch { store.save(s) }
                if (playing) {
                    PlaybackService.stop(context)
                } else {
                    PlaybackService.start(context, s)
                }
            },
            modifier = Modifier
                .fillMaxWidth()
                .height(72.dp),
            shapes = IconButtonDefaults.shapes(),
            colors = IconButtonDefaults.filledIconButtonColors(
                containerColor = if (playing) MaterialTheme.colorScheme.errorContainer
                else MaterialTheme.colorScheme.primaryContainer,
            ),
        ) {
            Text(
                if (playing) "Stop" else "Play",
                style = MaterialTheme.typography.headlineSmall,
            )
        }

        // ---- status card ----
        Card(modifier = Modifier.fillMaxWidth()) {
            Column(modifier = Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                Text("Status", style = MaterialTheme.typography.titleMedium)
                when (val s = state) {
                    is PlayerState.Idle -> StatusText("Idle")
                    is PlayerState.Connecting -> Row(verticalAlignment = Alignment.CenterVertically) {
                        CircularWavyProgressIndicator(modifier = Modifier.size(22.dp))
                        Spacer(Modifier.width(10.dp))
                        StatusText("Connecting…")
                    }
                    is PlayerState.Retrying -> StatusText("Reconnecting (${s.reason}) — attempt ${s.attempt}")
                    is PlayerState.Failed -> StatusText("Failed: ${s.reason}")
                    is PlayerState.Playing -> {
                        StatusText("Playing from “${s.server}”")
                        StatusText(s.format)
                        StatusText("buffer ${s.bufferedMs} ms · ${(s.bytesReceived / 1e6).format1()} MB")
                        StatusText("lost ${s.lostPackets} pkts" + (s.rttMs?.let { " · rtt $it ms" } ?: ""))
                    }
                }
            }
        }
    }
}

@Composable
private fun StatusText(text: String) {
    Text(text, style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
}

@Composable
private fun LabeledSlider(
    label: String,
    value: Float,
    min: Float,
    max: Float,
    fmt: String,
    display: ((Float) -> Any)? = null,
    onChange: (Float) -> Unit,
) {
    Column {
        Text("$label: " + (display?.invoke(value) ?: fmt.format(value)), style = MaterialTheme.typography.bodyMedium)
        Slider(value = value, valueRange = min..max, onValueChange = onChange)
    }
}

private fun Double.format1(): String = "%.1f".format(this)
