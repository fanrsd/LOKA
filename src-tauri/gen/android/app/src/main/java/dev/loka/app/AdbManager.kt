package dev.loka.app

import android.content.Context
import android.os.Build
import android.sun.security.x509.AlgorithmId
import android.sun.security.x509.CertificateAlgorithmId
import android.sun.security.x509.CertificateExtensions
import android.sun.security.x509.CertificateIssuerName
import android.sun.security.x509.CertificateSerialNumber
import android.sun.security.x509.CertificateSubjectName
import android.sun.security.x509.CertificateValidity
import android.sun.security.x509.CertificateVersion
import android.sun.security.x509.CertificateX509Key
import android.sun.security.x509.KeyIdentifier
import android.sun.security.x509.SubjectKeyIdentifierExtension
import android.sun.security.x509.X500Name
import android.sun.security.x509.X509CertImpl
import android.sun.security.x509.X509CertInfo
import io.github.muntashirakon.adb.AbsAdbConnectionManager
import java.io.File
import java.security.KeyFactory
import java.security.KeyPairGenerator
import java.security.PrivateKey
import java.security.cert.Certificate
import java.security.cert.CertificateFactory
import java.security.spec.PKCS8EncodedKeySpec
import java.util.Date
import java.util.Random

/** LOKA's own ADB client identity (RSA key + self-signed certificate in filesDir), the one Wireless debugging pairs with. */
class AdbManager private constructor(context: Context) : AbsAdbConnectionManager() {
  private val key: PrivateKey
  private val cert: Certificate

  init {
    setApi(Build.VERSION.SDK_INT)
    val keyFile = File(context.filesDir, "adb.key")
    val certFile = File(context.filesDir, "adb.crt")
    if (keyFile.exists() && certFile.exists()) {
      key = KeyFactory.getInstance("RSA").generatePrivate(PKCS8EncodedKeySpec(keyFile.readBytes()))
      cert = certFile.inputStream().use { CertificateFactory.getInstance("X.509").generateCertificate(it) }
    } else {
      val pair = KeyPairGenerator.getInstance("RSA").apply { initialize(2048) }.generateKeyPair()
      key = pair.private
      val name = X500Name("CN=LOKA")
      val from = Date()
      val until = Date(from.time + 100L * 365 * 86_400_000) // adbd doesn't check expiry; one key for the app's life
      val info = X509CertInfo().apply {
        set("version", CertificateVersion(2))
        set("serialNumber", CertificateSerialNumber(Random().nextInt() and Int.MAX_VALUE))
        set("algorithmID", CertificateAlgorithmId(AlgorithmId.get("SHA512withRSA")))
        set("subject", CertificateSubjectName(name))
        set("key", CertificateX509Key(pair.public))
        set("validity", CertificateValidity(from, until))
        set("issuer", CertificateIssuerName(name))
        set("extensions", CertificateExtensions().apply {
          set("SubjectKeyIdentifier", SubjectKeyIdentifierExtension(KeyIdentifier(pair.public).identifier))
        })
      }
      cert = X509CertImpl(info).apply { sign(key, "SHA512withRSA") }
      keyFile.writeBytes(key.encoded)
      certFile.writeBytes(cert.encoded)
    }
  }

  override fun getPrivateKey() = key
  override fun getCertificate() = cert
  override fun getDeviceName() = "LOKA"

  companion object {
    @Volatile private var instance: AdbManager? = null
    fun get(context: Context) = instance ?: synchronized(this) { instance ?: AdbManager(context.applicationContext).also { instance = it } }
  }
}
