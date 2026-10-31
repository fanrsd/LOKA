package dev.loka.app

import android.os.SystemClock
import android.view.InputDevice
import android.view.InputEvent
import android.view.KeyCharacterMap
import android.view.KeyEvent
import android.view.MotionEvent
import java.io.DataInputStream
import java.net.Socket

/**
 * Started by LOKA over its own ADB connection as the shell user:
 * `CLASSPATH=<apk> app_process / dev.loka.app.Injector <port> <token>`.
 * Connects back to the app on 127.0.0.1:<port>, proves itself with <token>, then turns 9-byte frames
 * (tag, i32, i32 little-endian) into input events: 1 move to (x, y), 2 button (b, down), 3 wheel (dx, dy),
 * 4 key (set-1 scancode, down), 5 wake the screen. The shell user may inject input; apps may not.
 */
object Injector {
  private val inject = run {
    // hidden API: InputManagerGlobal on Android 14+, InputManager before
    val (cls, getter) = try {
      Class.forName("android.hardware.input.InputManagerGlobal") to "getInstance"
    } catch (e: ClassNotFoundException) {
      Class.forName("android.hardware.input.InputManager") to "getInstance"
    }
    val manager = cls.getMethod(getter).invoke(null)
    val method = cls.getMethod("injectInputEvent", InputEvent::class.java, Int::class.javaPrimitiveType)
    val f: (InputEvent) -> Any? = { e -> method.invoke(manager, e, 0) } // 0 = INJECT_INPUT_EVENT_MODE_ASYNC
    f
  }

  private var x = 0f
  private var y = 0f
  private var buttons = 0
  private var downAt = 0L
  private var meta = 0

  @JvmStatic
  fun main(args: Array<String>) {
    val socket = Socket("127.0.0.1", args[0].toInt())
    socket.tcpNoDelay = true
    socket.getOutputStream().apply { write(args[1].toByteArray()); write('\n'.code); flush() }
    val input = DataInputStream(socket.getInputStream().buffered())
    val frame = ByteArray(9)
    while (true) {
      input.readFully(frame)
      val a = le(frame, 1)
      val b = le(frame, 5)
      when (frame[0].toInt()) {
        1 -> move(a.toFloat(), b.toFloat())
        2 -> button(a, b != 0)
        3 -> wheel(a, b)
        4 -> key(a, b != 0)
        5 -> tap(KeyEvent.KEYCODE_WAKEUP) // no-op when the screen is already on
      }
    }
  }

  private fun le(f: ByteArray, at: Int) =
    (f[at].toInt() and 0xFF) or ((f[at + 1].toInt() and 0xFF) shl 8) or ((f[at + 2].toInt() and 0xFF) shl 16) or (f[at + 3].toInt() shl 24)

  private fun motion(action: Int, actionButton: Int = 0, vscroll: Float = 0f, hscroll: Float = 0f) {
    val now = SystemClock.uptimeMillis()
    val props = MotionEvent.PointerProperties().apply { id = 0; toolType = MotionEvent.TOOL_TYPE_MOUSE }
    val coords = MotionEvent.PointerCoords().apply {
      this.x = this@Injector.x
      this.y = this@Injector.y
      pressure = if (buttons != 0) 1f else 0f
      size = 1f
      setAxisValue(MotionEvent.AXIS_VSCROLL, vscroll)
      setAxisValue(MotionEvent.AXIS_HSCROLL, hscroll)
    }
    val e = MotionEvent.obtain(if (buttons != 0) downAt else now, now, action, 1, arrayOf(props), arrayOf(coords), meta, buttons, 1f, 1f, 0, 0, InputDevice.SOURCE_MOUSE, 0)
    if (actionButton != 0) setActionButton(e, actionButton)
    inject(e)
    e.recycle()
  }

  private fun setActionButton(e: MotionEvent, b: Int) {
    MotionEvent::class.java.getMethod("setActionButton", Int::class.javaPrimitiveType).invoke(e, b)
  }

  private fun move(nx: Float, ny: Float) {
    x = nx
    y = ny
    motion(if (buttons != 0) MotionEvent.ACTION_MOVE else MotionEvent.ACTION_HOVER_MOVE)
  }

  private fun button(b: Int, down: Boolean) {
    // a phone has no context menus to right-click: right = Back, middle = Home (as scrcpy)
    when (b) {
      2 -> return if (down) tap(KeyEvent.KEYCODE_BACK) else Unit
      3 -> return if (down) tap(KeyEvent.KEYCODE_HOME) else Unit
    }
    val bit = when (b) {
      1 -> MotionEvent.BUTTON_PRIMARY
      4 -> MotionEvent.BUTTON_BACK
      5 -> MotionEvent.BUTTON_FORWARD
      else -> return
    }
    if (down) {
      val first = buttons == 0
      buttons = buttons or bit
      if (first) {
        downAt = SystemClock.uptimeMillis()
        motion(MotionEvent.ACTION_DOWN)
      }
      motion(MotionEvent.ACTION_BUTTON_PRESS, bit)
    } else {
      if (buttons and bit == 0) return
      buttons = buttons and bit.inv()
      motion(MotionEvent.ACTION_BUTTON_RELEASE, bit)
      if (buttons == 0) motion(MotionEvent.ACTION_UP)
    }
  }

  // 120 = one notch; Android scroll axes count notches, positive = up / right
  private fun wheel(dx: Int, dy: Int) = motion(MotionEvent.ACTION_SCROLL, vscroll = dy / 120f, hscroll = dx / 120f)

  private fun key(scan: Int, down: Boolean) {
    val code = KEYS[scan] ?: return
    val bit = MODIFIERS[code] ?: 0
    if (down) meta = meta or bit else meta = meta and bit.inv()
    keyEvent(code, down)
  }

  private fun keyEvent(code: Int, down: Boolean) {
    val now = SystemClock.uptimeMillis()
    inject(KeyEvent(now, now, if (down) KeyEvent.ACTION_DOWN else KeyEvent.ACTION_UP, code, 0, meta, KeyCharacterMap.VIRTUAL_KEYBOARD, 0, 0, InputDevice.SOURCE_KEYBOARD))
  }

  private fun tap(code: Int) {
    keyEvent(code, true)
    keyEvent(code, false)
  }

  private val MODIFIERS = mapOf(
    KeyEvent.KEYCODE_SHIFT_LEFT to (KeyEvent.META_SHIFT_ON or KeyEvent.META_SHIFT_LEFT_ON),
    KeyEvent.KEYCODE_SHIFT_RIGHT to (KeyEvent.META_SHIFT_ON or KeyEvent.META_SHIFT_RIGHT_ON),
    KeyEvent.KEYCODE_CTRL_LEFT to (KeyEvent.META_CTRL_ON or KeyEvent.META_CTRL_LEFT_ON),
    KeyEvent.KEYCODE_CTRL_RIGHT to (KeyEvent.META_CTRL_ON or KeyEvent.META_CTRL_RIGHT_ON),
    KeyEvent.KEYCODE_ALT_LEFT to (KeyEvent.META_ALT_ON or KeyEvent.META_ALT_LEFT_ON),
    KeyEvent.KEYCODE_ALT_RIGHT to (KeyEvent.META_ALT_ON or KeyEvent.META_ALT_RIGHT_ON),
    KeyEvent.KEYCODE_META_LEFT to (KeyEvent.META_META_ON or KeyEvent.META_META_LEFT_ON),
    KeyEvent.KEYCODE_META_RIGHT to (KeyEvent.META_META_ON or KeyEvent.META_META_RIGHT_ON),
  )

  /** PC/AT set-1 scancode (extended = 0xE0xx) → Android keycode. */
  private val KEYS: Map<Int, Int> = HashMap<Int, Int>().apply {
    val letters = mapOf('q' to 0x10, 'w' to 0x11, 'e' to 0x12, 'r' to 0x13, 't' to 0x14, 'y' to 0x15, 'u' to 0x16, 'i' to 0x17, 'o' to 0x18, 'p' to 0x19,
      'a' to 0x1E, 's' to 0x1F, 'd' to 0x20, 'f' to 0x21, 'g' to 0x22, 'h' to 0x23, 'j' to 0x24, 'k' to 0x25, 'l' to 0x26,
      'z' to 0x2C, 'x' to 0x2D, 'c' to 0x2E, 'v' to 0x2F, 'b' to 0x30, 'n' to 0x31, 'm' to 0x32)
    for ((c, s) in letters) put(s, KeyEvent.KEYCODE_A + (c - 'a'))
    for (d in 1..9) put(0x01 + d, KeyEvent.KEYCODE_0 + d)
    put(0x0B, KeyEvent.KEYCODE_0)
    for (f in 0..9) put(0x3B + f, KeyEvent.KEYCODE_F1 + f)
    putAll(mapOf(
      0x57 to KeyEvent.KEYCODE_F11, 0x58 to KeyEvent.KEYCODE_F12,
      0x01 to KeyEvent.KEYCODE_ESCAPE, 0x0C to KeyEvent.KEYCODE_MINUS, 0x0D to KeyEvent.KEYCODE_EQUALS, 0x0E to KeyEvent.KEYCODE_DEL,
      0x0F to KeyEvent.KEYCODE_TAB, 0x1A to KeyEvent.KEYCODE_LEFT_BRACKET, 0x1B to KeyEvent.KEYCODE_RIGHT_BRACKET, 0x1C to KeyEvent.KEYCODE_ENTER,
      0x1D to KeyEvent.KEYCODE_CTRL_LEFT, 0x27 to KeyEvent.KEYCODE_SEMICOLON, 0x28 to KeyEvent.KEYCODE_APOSTROPHE, 0x29 to KeyEvent.KEYCODE_GRAVE,
      0x2A to KeyEvent.KEYCODE_SHIFT_LEFT, 0x2B to KeyEvent.KEYCODE_BACKSLASH, 0x33 to KeyEvent.KEYCODE_COMMA, 0x34 to KeyEvent.KEYCODE_PERIOD,
      0x35 to KeyEvent.KEYCODE_SLASH, 0x36 to KeyEvent.KEYCODE_SHIFT_RIGHT, 0x38 to KeyEvent.KEYCODE_ALT_LEFT, 0x39 to KeyEvent.KEYCODE_SPACE,
      0x3A to KeyEvent.KEYCODE_CAPS_LOCK, 0xE01C to KeyEvent.KEYCODE_NUMPAD_ENTER, 0xE01D to KeyEvent.KEYCODE_CTRL_RIGHT,
      0xE038 to KeyEvent.KEYCODE_ALT_RIGHT, 0xE05B to KeyEvent.KEYCODE_META_LEFT, 0xE05C to KeyEvent.KEYCODE_META_RIGHT,
      0xE048 to KeyEvent.KEYCODE_DPAD_UP, 0xE050 to KeyEvent.KEYCODE_DPAD_DOWN, 0xE04B to KeyEvent.KEYCODE_DPAD_LEFT, 0xE04D to KeyEvent.KEYCODE_DPAD_RIGHT,
      0xE047 to KeyEvent.KEYCODE_MOVE_HOME, 0xE04F to KeyEvent.KEYCODE_MOVE_END, 0xE049 to KeyEvent.KEYCODE_PAGE_UP, 0xE051 to KeyEvent.KEYCODE_PAGE_DOWN,
      0xE052 to KeyEvent.KEYCODE_INSERT, 0xE053 to KeyEvent.KEYCODE_FORWARD_DEL,
    ))
  }
}
