package io.audiosharers.client

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

/** mDNS/NSD discovery for `_audiosharers._tcp` servers advertised by `ashr serve`. */
object Discovery {
    private const val SERVICE_TYPE = "_audiosharers._tcp."

    data class Server(val name: String, val host: String, val port: Int)

    /** Blocking browse; call from a coroutine on Dispatchers.IO. */
    suspend fun browse(context: Context, timeoutMs: Long): List<Server> =
        withContext(Dispatchers.IO) {
            val nsd = context.getSystemService(Context.NSD_SERVICE) as NsdManager
            val found = LinkedHashMap<String, Server>()
            val done = object {
                @Volatile var finished = false
            }

            val listener = object : NsdManager.DiscoveryListener {
                override fun onDiscoveryStarted(serviceType: String) {}
                override fun onStartDiscoveryFailed(serviceType: String, errorCode: Int) {
                    done.finished = true
                }
                override fun onStopDiscoveryFailed(serviceType: String, errorCode: Int) {}
                override fun onDiscoveryStopped(serviceType: String) {}
                override fun onServiceFound(serviceInfo: NsdServiceInfo) {
                    nsd.resolveService(
                        serviceInfo,
                        object : NsdManager.ResolveListener {
                            override fun onResolveFailed(info: NsdServiceInfo, errorCode: Int) {}
                            override fun onServiceResolved(info: NsdServiceInfo) {
                                val host = info.host?.hostAddress ?: return
                                val name = info.serviceName ?: "server"
                                synchronized(found) {
                                    found[host] = Server(name, host, info.port)
                                }
                            }
                        }
                    )
                }
                override fun onServiceLost(serviceInfo: NsdServiceInfo) {}
            }

            nsd.discoverServices(SERVICE_TYPE, NsdManager.PROTOCOL_DNS_SD, listener)

            // Poll until the timeout elapses; NSD has no async completion API.
            val deadline = System.currentTimeMillis() + timeoutMs
            while (System.currentTimeMillis() < deadline && !done.finished) {
                Thread.sleep(150)
            }
            runCatching { nsd.stopServiceDiscovery(listener) }
            synchronized(found) { found.values.toList() }
        }
}
