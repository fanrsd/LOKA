package dev.loka.app

import android.Manifest
import android.app.Activity
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.Path
import android.graphics.PixelFormat
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.provider.Settings
import android.view.Gravity
import android.view.View
import android.view.WindowManager
import androidx.core.app.NotificationCompat
import androidx.core.app.RemoteInput
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import io.github.muntashirakon.adb.AdbPairingRequiredException
import io.github.muntashirakon.adb.AdbStream
import io.github.muntashirakon.adb.android.AdbMdns
import java.security.Security
import org.conscrypt.Conscrypt

@InvokeArg class PageArgs { var page: String = "developer" }
@InvokeArg class CodeArgs { var code: String = "" }
@InvokeArg class StartArgs { var port: Int = 0; var token: String = "" }
@InvokeArg class CursorArgs { var x: Double = 0.0; var y: Double = 0.0; var show: Boolean = false }

/**
 * Android side of "let computers control this phone": Wireless debugging pairing with LOKA's own ADB client,
 * starting [Injector] as the shell user, the cursor overlay, and keeping LOKA reachable ([ReachableService]).
 */
@TauriPlugin
class LokaPlugin(private val activity: Activity) : Plugin(activity) {
  private val ctx: Context = activity.applicationContext
  private val prefs by lazy { ctx.getSharedPreferences("loka", Context.MODE_PRIVATE) }
  private val main = Handler(Looper.getMainLooper())
  @Volatile private var injector: AdbStream? = null
  private var pairingPort = 0
  private var mdns: AdbMdns? = null

  override fun load(webView: android.webkit.WebView) {
    super.load(webView)
    instance = this
    Security.insertProviderAt(Conscrypt.newProvider(), 1) // TLS 1.3 + key export for ADB pairing on every Android version
    val wanted = listOfNotNull(
      if (Build.VERSION.SDK_INT >= 37) Manifest.permission.ACCESS_LOCAL_NETWORK else null,
      if (Build.VERSION.SDK_INT >= 33) Manifest.permission.POST_NOTIFICATIONS else null,
    ).filter { ctx.checkSelfPermission(it) != PackageManager.PERMISSION_GRANTED }
    if (wanted.isNotEmpty()) activity.requestPermissions(wanted.toTypedArray(), 1)
    ReachableService.start(ctx)
  }

  private fun global(name: String) = Settings.Global.getInt(ctx.contentResolver, name, 0) == 1

  @Command
  fun status(invoke: Invoke) {
    invoke.resolve(JSObject().apply {
      put("sdk", Build.VERSION.SDK_INT)
      put("maker", Build.MANUFACTURER.lowercase())
      put("devOptions", global(Settings.Global.DEVELOPMENT_SETTINGS_ENABLED))
      put("wirelessDebugging", global("adb_wifi_enabled"))
      put("paired", prefs.getBoolean("paired", false))
      put("running", injector?.isClosed == false)
    })
  }

  @Command
  fun openSettings(invoke: Invoke) {
    val page = invoke.parseArgs(PageArgs::class.java).page
    val intent = when (page) {
      "about" -> Intent(Settings.ACTION_DEVICE_INFO_SETTINGS)
      "security" -> Intent(Settings.ACTION_SECURITY_SETTINGS)
      // Settings scrolls to and highlights the Wireless debugging row
      "wireless" -> Intent(Settings.ACTION_APPLICATION_DEVELOPMENT_SETTINGS).putExtra(":settings:fragment_args_key", "toggle_adb_wireless")
        .putExtra(":settings:show_fragment_args", Bundle().apply { putString(":settings:fragment_args_key", "toggle_adb_wireless") })
      else -> Intent(Settings.ACTION_APPLICATION_DEVELOPMENT_SETTINGS)
    }.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
    try {
      activity.startActivity(intent)
      invoke.resolve()
    } catch (e: Exception) {
      invoke.reject("Android has no such settings page here")
    }
  }

  /** A notification with a reply box: the code dialog in Settings closes as soon as the user switches apps. */
  @Command
  fun askCode(invoke: Invoke) {
    watchPairingPort()
    val nm = ctx.getSystemService(NotificationManager::class.java)
    if (Build.VERSION.SDK_INT >= 26) nm.createNotificationChannel(NotificationChannel(PAIR_CHANNEL, "Pairing", NotificationManager.IMPORTANCE_HIGH))
    val reply = PendingIntent.getBroadcast(ctx, 0, Intent(ctx, PairReplyReceiver::class.java),
      PendingIntent.FLAG_UPDATE_CURRENT or (if (Build.VERSION.SDK_INT >= 31) PendingIntent.FLAG_MUTABLE else 0))
    val action = NotificationCompat.Action.Builder(0, "Enter code", reply)
      .addRemoteInput(RemoteInput.Builder(PairReplyReceiver.KEY).setLabel("6-digit code").build()).build()
    nm.notify(PAIR_NOTIFICATION, NotificationCompat.Builder(ctx, PAIR_CHANNEL)
      .setSmallIcon(R.mipmap.ic_launcher)
      .setContentTitle("Type the Wi-Fi pairing code here")
      .setContentText("Settings shows it under Pair device with pairing code")
      .setPriority(NotificationCompat.PRIORITY_HIGH)
      .addAction(action).build())
    invoke.resolve()
  }

  @Command
  fun pairCode(invoke: Invoke) {
    pair(invoke.parseArgs(CodeArgs::class.java).code)
    invoke.resolve()
  }

  private fun watchPairingPort() {
    if (mdns?.isRunning == true) return
    mdns = AdbMdns(ctx, AdbMdns.SERVICE_TYPE_TLS_PAIRING) { _, port -> if (port > 0) pairingPort = port }.also { it.start() }
  }

  /** Pairs with this phone's own adbd; the answer comes as the `pair` event. */
  fun pair(code: String) {
    Thread {
      val result = JSObject()
      try {
        if (pairingPort == 0) throw IllegalStateException("Open Pair device with pairing code in Settings first")
        AdbManager.get(ctx).pair(pairingPort, code.trim())
        prefs.edit().putBoolean("paired", true).apply()
        mdns?.stop()
        result.put("ok", true)
      } catch (e: Throwable) {
        android.util.Log.w("LOKA", "pairing failed", e)
        result.put("ok", false)
        result.put("error", e.message ?: "pairing failed")
      }
      ctx.getSystemService(NotificationManager::class.java).cancel(PAIR_NOTIFICATION)
      trigger("pair", result)
    }.start()
  }

  /** Connect to our own adbd, grant what LOKA needs, and start [Injector] dialing back to 127.0.0.1:`port`. */
  @Command
  fun start(invoke: Invoke) {
    val args = invoke.parseArgs(StartArgs::class.java)
    Thread {
      try {
        launch(args)
        invoke.resolve()
      } catch (e: AdbPairingRequiredException) {
        prefs.edit().putBoolean("paired", false).apply()
        invoke.reject("pair again")
      } catch (e: Exception) {
        invoke.reject(e.message ?: e.toString())
      }
    }.start()
  }

  private fun launch(args: StartArgs) {
    val adb = AdbManager.get(ctx)
    // after a restart or a Wi-Fi change Android turns Wireless debugging off; LOKA may turn it back on
    if (!global("adb_wifi_enabled") && ctx.checkSelfPermission(Manifest.permission.WRITE_SECURE_SETTINGS) == PackageManager.PERMISSION_GRANTED) {
      Settings.Global.putInt(ctx.contentResolver, "adb_wifi_enabled", 1)
      Thread.sleep(1500)
    }
    if (!adb.isConnected && !adb.connectTls(ctx, 10_000)) throw IllegalStateException("Wireless debugging isn't answering. Is it on?")
    shell(adb, "pm grant ${ctx.packageName} android.permission.WRITE_SECURE_SETTINGS; appops set ${ctx.packageName} SYSTEM_ALERT_WINDOW allow")
    injector?.close()
    val apk = ctx.applicationInfo.sourceDir
    val stream = adb.openStream("shell:CLASSPATH=$apk app_process / ${Injector::class.java.name} ${args.port} ${args.token}")
    injector = stream
    Thread {
      // the injector prints nothing; this read ends when it (or the ADB link) dies
      val buf = ByteArray(256)
      try { while (stream.openInputStream().read(buf) >= 0) Unit } catch (_: Exception) {}
      trigger("injector", JSObject().apply { put("running", false) })
      // adbd restarts (Wi-Fi change, Wireless debugging toggled) kill it: start it again until it runs,
      // unless a newer `start` replaced it; a computer's cursor would otherwise move in with no input
      while (injector === stream) {
        Thread.sleep(3000)
        try {
          try { adb.disconnect() } catch (_: java.io.IOException) {} // the old link is dead even if it doesn't know yet
          launch(args)
          trigger("injector", JSObject().apply { put("running", true) })
        } catch (e: AdbPairingRequiredException) {
          prefs.edit().putBoolean("paired", false).apply()
          break
        } catch (e: Exception) {
          android.util.Log.w("LOKA", "injector restart failed", e)
        }
      }
    }.start()
  }

  /** Runs `cmd` as the shell user and waits for it to finish (libadb reports the end as "Stream closed"). */
  private fun shell(adb: AdbManager, cmd: String) {
    val s = adb.openStream("shell:$cmd")
    try {
      val b = ByteArray(1024)
      val i = s.openInputStream()
      while (i.read(b) >= 0) Unit
    } catch (_: java.io.IOException) {
    } finally {
      try { s.close() } catch (_: java.io.IOException) {}
    }
  }

  private var pointer: View? = null

  /** The computer's cursor, drawn over every app: injected mouse events show none. */
  @Command
  fun cursor(invoke: Invoke) {
    val a = invoke.parseArgs(CursorArgs::class.java)
    main.post {
      val wm = ctx.getSystemService(WindowManager::class.java)
      if (!a.show) {
        pointer?.let { wm.removeView(it) }
        pointer = null
        return@post
      }
      if (!Settings.canDrawOverlays(ctx)) return@post
      val lp = (pointer?.layoutParams as? WindowManager.LayoutParams) ?: WindowManager.LayoutParams(
        ARROW_W, ARROW_H,
        if (Build.VERSION.SDK_INT >= 26) WindowManager.LayoutParams.TYPE_APPLICATION_OVERLAY else @Suppress("DEPRECATION") WindowManager.LayoutParams.TYPE_PHONE,
        WindowManager.LayoutParams.FLAG_NOT_TOUCHABLE or WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE or WindowManager.LayoutParams.FLAG_LAYOUT_IN_SCREEN or WindowManager.LayoutParams.FLAG_LAYOUT_NO_LIMITS,
        PixelFormat.TRANSLUCENT,
      ).apply { gravity = Gravity.TOP or Gravity.START }
      lp.x = a.x.toInt()
      lp.y = a.y.toInt()
      val v = pointer
      if (v == null) pointer = Arrow(ctx).also { wm.addView(it, lp) } else wm.updateViewLayout(v, lp)
    }
    invoke.resolve()
  }

  private class Arrow(c: Context) : View(c) {
    private val fill = Paint(Paint.ANTI_ALIAS_FLAG).apply { color = Color.WHITE }
    private val edge = Paint(Paint.ANTI_ALIAS_FLAG).apply { color = Color.BLACK; style = Paint.Style.STROKE; strokeWidth = 3f; strokeJoin = Paint.Join.ROUND }
    private val path = Path().apply {
      val s = ARROW_H / 30f
      moveTo(2 * s, 2 * s); lineTo(2 * s, 24 * s); lineTo(8 * s, 18.5f * s); lineTo(12 * s, 27.5f * s); lineTo(16 * s, 25.7f * s); lineTo(12 * s, 17 * s); lineTo(20 * s, 17 * s); close()
    }
    override fun onDraw(canvas: Canvas) {
      canvas.drawPath(path, fill)
      canvas.drawPath(path, edge)
    }
  }

  companion object {
    @Volatile var instance: LokaPlugin? = null
    const val PAIR_CHANNEL = "pairing"
    const val PAIR_NOTIFICATION = 2
    const val ARROW_W = 48
    const val ARROW_H = 66
  }
}
