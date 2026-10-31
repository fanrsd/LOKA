package dev.loka.app

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.wifi.WifiManager
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat

/**
 * Keeps LOKA running while another app is in front (computers find it, drive it, send it files) and holds
 * the Wi-Fi multicast lock: without it most phones drop discovery's multicast packets.
 */
class ReachableService : Service() {
  private var multicast: WifiManager.MulticastLock? = null

  override fun onBind(intent: Intent?): IBinder? = null

  override fun onCreate() {
    super.onCreate()
    val nm = getSystemService(NotificationManager::class.java)
    if (Build.VERSION.SDK_INT >= 26) nm.createNotificationChannel(NotificationChannel(CHANNEL, "Reachable", NotificationManager.IMPORTANCE_MIN))
    val open = PendingIntent.getActivity(this, 0, Intent(this, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE)
    val n: Notification = NotificationCompat.Builder(this, CHANNEL)
      .setSmallIcon(R.mipmap.ic_launcher)
      .setContentTitle("LOKA is on")
      .setContentText("Your computers can find this phone")
      .setContentIntent(open)
      .setOngoing(true)
      .build()
    if (Build.VERSION.SDK_INT >= 29) startForeground(1, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE) else startForeground(1, n)
    multicast = applicationContext.getSystemService(WifiManager::class.java).createMulticastLock("loka").apply { setReferenceCounted(false); acquire() }
  }

  override fun onDestroy() {
    multicast?.release()
    super.onDestroy()
  }

  companion object {
    private const val CHANNEL = "reachable"
    fun start(c: Context) {
      val i = Intent(c, ReachableService::class.java)
      if (Build.VERSION.SDK_INT >= 26) c.startForegroundService(i) else c.startService(i)
    }
  }
}
