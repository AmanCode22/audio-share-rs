package io.audiosharers.client

import java.io.DataInputStream
import java.io.DataOutputStream
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * `ASR2` wire protocol — byte-exact mirror of crates/ash-core/src/protocol.rs.
 *
 * TCP frame: magic "ASR2" | version u16le | type u8 | flags u8 | len u32le | payload
 * UDP audio: session u32le | seq u32le | pcm...
 * UDP registration/keepalive: session u32le
 */
object Protocol {
    // Kotlin forbids `const` for ByteArray; a plain val in the object is fine.
    val MAGIC: ByteArray = byteArrayOf('A'.code.toByte(), 'S'.code.toByte(), 'R'.code.toByte(), '2'.code.toByte())
    const val VERSION: Int = 2

    const val TYPE_HELLO: Int = 1
    const val TYPE_START: Int = 2
    const val TYPE_STARTED: Int = 3
    const val TYPE_AUDIO: Int = 4
    const val TYPE_PING: Int = 5
    const val TYPE_PONG: Int = 6
    const val TYPE_STATS: Int = 7
    const val TYPE_STOP: Int = 8

    const val FLAG_TCP_DATA: Int = 1

    const val HEADER_LEN: Int = 12
    const val MTU_DATA_MAX: Int = 1464

    data class AudioFormat(val encoding: Int, val channels: Int, val sampleRate: Int) {
        val bytesPerFrame: Int get() = when (encoding) {
            Encoding.S16LE -> 2 * channels
            Encoding.F32LE -> 4 * channels
            else -> channels
        }
        val bytesPerSecond: Int get() = bytesPerFrame * sampleRate
    }

    object Encoding {
        const val S16LE = 0
        const val F32LE = 1
    }

    data class Hello(val format: AudioFormat, val blockMs: Int, val name: String)

    class ProtocolException(message: String) : Exception(message)

    /** Reads exactly one frame; returns (type, flags, payload). */
    fun readFrame(input: DataInputStream): Triple<Int, Int, ByteArray> {
        val head = ByteArray(HEADER_LEN)
        input.readFully(head)
        for (i in 0..3) {
            if (head[i] != MAGIC[i]) throw ProtocolException("bad magic (not an ASR2 stream)")
        }
        val bb = ByteBuffer.wrap(head, 4, 8).order(ByteOrder.LITTLE_ENDIAN)
        val version = bb.short.toInt() and 0xFFFF
        if (version != VERSION) throw ProtocolException("unsupported protocol version $version")
        val type = bb.get().toInt() and 0xFF
        val flags = bb.get().toInt() and 0xFF
        val len = bb.int
        if (len < 0 || len > 4 * 1024 * 1024) throw ProtocolException("bad payload length $len")
        val payload = ByteArray(len)
        input.readFully(payload)
        return Triple(type, flags, payload)
    }

    fun writeFrame(out: DataOutputStream, type: Int, flags: Int, payload: ByteArray) {
        val head = ByteBuffer.allocate(HEADER_LEN).order(ByteOrder.LITTLE_ENDIAN)
        head.put(MAGIC)
        head.putShort(VERSION.toShort())
        head.put(type.toByte())
        head.put(flags.toByte())
        head.putInt(payload.size)
        out.write(head.array())
        out.write(payload)
        out.flush()
    }

    fun parseHello(payload: ByteArray): Hello {
        if (payload.size < 12) throw ProtocolException("hello too short")
        val bb = ByteBuffer.wrap(payload).order(ByteOrder.LITTLE_ENDIAN)
        val encoding = bb.short.toInt() and 0xFFFF
        val channels = bb.short.toInt() and 0xFFFF
        val sampleRate = bb.int
        val blockMs = bb.short.toInt() and 0xFFFF
        val nameLen = bb.short.toInt() and 0xFFFF
        if (payload.size < 12 + nameLen) throw ProtocolException("hello name truncated")
        val name = String(payload, 12, nameLen, Charsets.UTF_8)
        return Hello(AudioFormat(encoding, channels, sampleRate), blockMs, name)
    }

    fun startFrame(tcpData: Boolean): Pair<Int, ByteArray> =
        Pair(if (tcpData) FLAG_TCP_DATA else 0, ByteArray(0))

    fun startedSession(payload: ByteArray): Int {
        if (payload.size < 4) throw ProtocolException("started too short")
        return ByteBuffer.wrap(payload).order(ByteOrder.LITTLE_ENDIAN).int
    }

    fun tcpAudioSeq(payload: ByteArray): Int =
        ByteBuffer.wrap(payload).order(ByteOrder.LITTLE_ENDIAN).int

    fun tcpAudioPcm(payload: ByteArray): ByteArray = payload.copyOfRange(4, payload.size)

    fun udpAudio(datagram: ByteArray): Triple<Int, Int, ByteArray>? {
        if (datagram.size < 8) return null
        val bb = ByteBuffer.wrap(datagram).order(ByteOrder.LITTLE_ENDIAN)
        val session = bb.int
        val seq = bb.int
        return Triple(session, seq, datagram.copyOfRange(8, datagram.size))
    }
}
