package dev.runo.runo_app

import android.content.Context
import android.net.wifi.WifiManager
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugin.common.MethodChannel

class MainActivity : FlutterActivity() {
    private var lock: WifiManager.MulticastLock? = null

    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        MethodChannel(flutterEngine.dartExecutor.binaryMessenger, "dev.runo/multicast")
            .setMethodCallHandler { call, result ->
                when (call.method) {
                    "acquire" -> {
                        val held = lock ?: (applicationContext
                            .getSystemService(Context.WIFI_SERVICE) as WifiManager)
                            .createMulticastLock("runo").also {
                                it.setReferenceCounted(true)
                                lock = it
                            }
                        held.acquire()
                        result.success(null)
                    }
                    "release" -> {
                        lock?.let { if (it.isHeld) it.release() }
                        result.success(null)
                    }
                    else -> result.notImplemented()
                }
            }
    }
}
