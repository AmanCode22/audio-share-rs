package io.audiosharers.client

import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioManager
import android.media.AudioTrack
import android.media.audiofx.LoudnessEnhancer
import android.os.SystemClock
import android.util.Log
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import java.io.DataInputStream
import java.io.DataOutputStream
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.Socket
import java.net.SocketTimeoutException
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicLong

/** UI-facing playback state. */
sealed interface PlayerState {
    data object Idle : PlayerState
    data object Connecting : PlayerState
    data class Playing(
        val server: String,
        val format: String,
        val bufferedMs: Int,
        val bytesReceived: Long,
        val lostPackets: Long,
        val rttMs: Long?,
    ) : PlayerState

    data class Retrying(val reason: String, val attempt: Int) : PlayerState
    data class Failed(val reason: String) : PlayerState
}

/**
 * TCP control + UDP data client for the `ASR2` protocol, with an AudioTrack
 * sink, jitter buffering, loss accounting and RTT probing.
 */
class NetClient(
    private val host: String,
    private val port: Int,
    private val tcpData: Boolean,
    private val bufferMs: Int,
    private val volume: Float,
    private val loudnessGainMb: Int,
    private val onState: (PlayerState) -> Unit,
) {
    private val tag = "NetClient"
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private var job: Job? = null

    private val bytesReceived = AtomicLong(0)
    private val lostPackets = AtomicLong(0)
    private val rttMs = AtomicLong(-1)
    private val lastSeq = AtomicInteger(-1)
    private val bufferedMs = AtomicInteger(0)

    @Volatile private var stopped = false

    fun start() {
        if (job?.isActive == true) return
        stopped = false
        onState(PlayerState.Connecting)
        job = scope.launch {
            var attempt = 0
            var backoff = 1500L
            while (!stopped && isActive) {
                try {
                    runOnce()
                    break // graceful end
                } catch (e: Exception) {
                    if (stopped) break
                    attempt += 1
                    onState(PlayerState.Retrying(e.message ?: e.javaClass.simpleName, attempt))
                    Log.w(tag, "disconnected: ${e.message}", e)
                    delay(backoff)
                    backoff = (backoff * 2).coerceAtMost(8000)
                }
            }
        }
    }

    fun stop() {
        stopped = true
        job?.cancel()
        job = null
        onState(PlayerState.Idle)
    }

    private fun emitPlaying(hello: Protocol.Hello) {
        val formatName = when (hello.format.encoding) {
            Protocol.Encoding.S16LE -> "PCM 16-bit"
            Protocol.Encoding.F32LE -> "PCM float"
            else -> "PCM"
        }
        onState(
            PlayerState.Playing(
                server = hello.name,
                format = "${hello.format.sampleRate} Hz · ${hello.format.channels} ch · $formatName",
                bufferedMs = bufferedMs.get(),
                bytesReceived = bytesReceived.get(),
                lostPackets = lostPackets.get(),
                rttMs = rttMs.get().takeIf { it >= 0 },
            )
        )
    }

    private fun runOnce() {
        Socket().use { socket ->
            socket.tcpNoDelay = true
            socket.connect(InetSocketAddress(host, port), 5000)

            val input = DataInputStream(socket.getInputStream().buffered())
            val output = DataOutputStream(socket.getOutputStream().buffered())

            // 1) Hello
            val (helloType, _, helloPayload) = Protocol.readFrame(input)
            if (helloType != Protocol.TYPE_HELLO) {
                throw Protocol.ProtocolException("expected Hello, got type $helloType")
            }
            val hello = Protocol.parseHello(helloPayload)
            Log.i(tag, "server '${hello.name}': ${hello.format.sampleRate} Hz, ${hello.format.channels} ch (block ${hello.blockMs} ms)")

            // 2) Start
            val (startFlags, startPayload) = Protocol.startFrame(tcpData)
            Protocol.writeFrame(output, Protocol.TYPE_START, startFlags, startPayload)

            // 3) Started (a Ping may race ahead of it — tolerate either order)
            var session = -1
            while (true) {
                val (t, _, p) = Protocol.readFrame(input)
                when (t) {
                    Protocol.TYPE_STARTED -> {
                        session = Protocol.startedSession(p)
                        break
                    }
                    Protocol.TYPE_PING -> {
                        if (p.size == 8) Protocol.writeFrame(output, Protocol.TYPE_PONG, 0, p)
                    }
                    else -> Log.d(tag, "ignoring frame type $t during handshake")
                }
            }
            Log.i(tag, "playing — session $session, ${if (tcpData) "TCP" else "UDP"} mode")

            // 4) Audio sink
            val track = buildTrack(hello.format) ?: throw Protocol.ProtocolException("unsupported audio format")
            var enhancer: LoudnessEnhancer? = null
            try {
                if (loudnessGainMb > 0) {
                    enhancer = LoudnessEnhancer(track.audioSessionId).also {
                        it.setTargetGain(loudnessGainMb.toFloat())
                        it.setEnabled(true)
                    }
                }
                track.play()

                // Data path: UDP receiver or TCP audio reader.
                val udpTask = if (!tcpData) {
                    launchUdp(session, track, hello)
                } else null

                val statsTask = launchStats(output, hello)
                try {
                    // TCP loop: control frames (+ audio when tcpData).
                    while (!stopped) {
                        val (t, _, p) = Protocol.readFrame(input)
                        when (t) {
                            Protocol.TYPE_AUDIO -> {
                                if (tcpData) {
                                    val seq = Protocol.tcpAudioSeq(p)
                                    writePcm(track, seq, Protocol.tcpAudioPcm(p), hello.format)
                                    emitPlaying(hello)
                                }
                            }
                            Protocol.TYPE_PING -> {
                                if (p.size == 8) Protocol.writeFrame(output, Protocol.TYPE_PONG, 0, p)
                            }
                            Protocol.TYPE_STATS -> {
                                if (p.size == 8) {
                                    val sent = ByteBufferCompat.getLong(p)
                                    rttMs.set(SystemClock.elapsedRealtime() - sent)
                                }
                                emitPlaying(hello)
                            }
                            Protocol.TYPE_STOP -> return
                        }
                    }
                } finally {
                    udpTask?.cancel()
                    statsTask.cancel()
                }
            } finally {
                try { track.pause(); track.flush() } catch (_: Exception) {}
                track.release()
                runCatching { enhancer?.release() }
            }
        }
    }

    private fun launchStats(output: DataOutputStream, hello: Protocol.Hello): Job =
        scope.launch {
            while (!stopped && isActive) {
                try {
                    val probe = ByteBuffer.allocate(8).order(java.nio.ByteOrder.LITTLE_ENDIAN)
                        .putLong(SystemClock.elapsedRealtime()).array()
                    Protocol.writeFrame(output, Protocol.TYPE_STATS, 0, probe)
                } catch (e: Exception) {
                    Log.d(tag, "stats probe failed: ${e.message}")
                }
                delay(3000)
                emitPlaying(hello)
            }
        }

    private fun launchUdp(session: Int, track: AudioTrack, hello: Protocol.Hello): Job =
        scope.launch {
            DatagramSocket().use { sock ->
                sock.reuseAddress = true
                val addr = InetAddress.getByName(host)
                val token = ByteBuffer.allocate(4)
                    .order(java.nio.ByteOrder.LITTLE_ENDIAN)
                    .putInt(session).array()

                // keepalive + receive loop
                var lastKeepalive = 0L
                val buf = ByteArray(2048)
                val packet = DatagramPacket(buf, buf.size)
                sock.soTimeout = 1000
                while (!stopped && isActive) {
                    val now = SystemClock.elapsedRealtime()
                    if (now - lastKeepalive >= 1000) {
                        sock.send(DatagramPacket(token, token.size, addr, port))
                        lastKeepalive = now
                    }
                    try {
                        sock.receive(packet)
                    } catch (_: SocketTimeoutException) {
                        continue
                    }
                    val parsed = Protocol.udpAudio(buf.copyOf(packet.length)) ?: continue
                    val (s, seq, pcm) = parsed
                    if (s != session) continue
                    // loss accounting
                    val last = lastSeq.get()
                    if (last >= 0) {
                        val diff = (seq - last) and 0xFFFFFFFFL.toInt()
                        if (diff in 2..8191) lostPackets.addAndGet((diff - 1).toLong())
                    }
                    lastSeq.set(seq)
                    writePcm(track, seq, pcm, hello.format)
                    emitPlaying(hello)
                }
            }
        }

    /** Jitter-buffer style write: track buffer capacity bounds our latency. */
    private fun writePcm(track: AudioTrack, seq: Int, pcm: ByteArray, format: Protocol.AudioFormat) {
        bytesReceived.addAndGet(pcm.size.toLong())
        val capacityMs = track.bufferSizeInFrames * 1000 / format.sampleRate
        bufferedMs.set(capacityMs.coerceAtMost(bufferMs * 2))
        track.write(pcm, 0, pcm.size, AudioTrack.WRITE_BLOCKING)
    }

    private fun buildTrack(format: Protocol.AudioFormat): AudioTrack? {
        val encoding = when (format.encoding) {
            Protocol.Encoding.S16LE -> AudioFormat.ENCODING_PCM_16BIT
            Protocol.Encoding.F32LE -> AudioFormat.ENCODING_PCM_FLOAT
            else -> return null
        }
        val channelMask = when (format.channels) {
            1 -> AudioFormat.CHANNEL_OUT_MONO
            2 -> AudioFormat.CHANNEL_OUT_STEREO
            3 -> AudioFormat.CHANNEL_OUT_STEREO or AudioFormat.CHANNEL_OUT_FRONT_CENTER
            4 -> AudioFormat.CHANNEL_OUT_QUAD
            5 -> AudioFormat.CHANNEL_OUT_QUAD or AudioFormat.CHANNEL_OUT_FRONT_CENTER
            6 -> AudioFormat.CHANNEL_OUT_5POINT1
            7 -> AudioFormat.CHANNEL_OUT_5POINT1 or AudioFormat.CHANNEL_OUT_BACK_CENTER
            8 -> AudioFormat.CHANNEL_OUT_7POINT1_SURROUND
            else -> return null
        }
        val minBuffer = AudioTrack.getMinBufferSize(format.sampleRate, channelMask, encoding)
        if (minBuffer <= 0) return null
        // Scale the buffer toward the requested latency target.
        val targetBytes = format.bytesPerSecond * bufferMs / 1000
        val size = (targetBytes.coerceAtLeast(minBuffer)).coerceAtMost(minBuffer * 8)
        return AudioTrack.Builder()
            .setAudioAttributes(
                AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_MEDIA)
                    .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
                    .build()
            )
            .setAudioFormat(
                AudioFormat.Builder()
                    .setEncoding(encoding)
                    .setChannelMask(channelMask)
                    .setSampleRate(format.sampleRate)
                    .build()
            )
            .setTransferMode(AudioTrack.MODE_STREAM)
            .setBufferSizeInBytes(size)
            .setPerformanceMode(AudioTrack.PERFORMANCE_MODE_LOW_LATENCY)
            .build()
            .also { it.setVolume(volume.coerceIn(0f, 1f)) }
    }

    private object ByteBufferCompat {
        fun getLong(bytes: ByteArray): Long =
            java.nio.ByteBuffer.wrap(bytes).order(java.nio.ByteOrder.LITTLE_ENDIAN).long
    }
}
