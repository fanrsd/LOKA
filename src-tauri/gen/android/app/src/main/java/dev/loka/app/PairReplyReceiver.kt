package dev.loka.app

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import androidx.core.app.RemoteInput

/** The pairing code typed into LOKA's notification while Settings shows it. */
class PairReplyReceiver : BroadcastReceiver() {
  override fun onReceive(context: Context, intent: Intent) {
    val code = RemoteInput.getResultsFromIntent(intent)?.getCharSequence(KEY)?.toString() ?: return
    LokaPlugin.instance?.pair(code)
  }

  companion object {
    const val KEY = "code"
  }
}
