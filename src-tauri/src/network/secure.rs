//! Every TCP link (input sessions, pairing, files) runs over TLS 1.3 with a self-signed certificate per
//! device. A device *is* its certificate: the fingerprint (SHA-256 of the certificate) is what discovery
//! announces, what pairing stores, and what each side checks on every connection. Pairing shows a 6-digit
//! code made from both fingerprints, so a device pretending to be another on the LAN can't be paired by
//! mistake, and after pairing it can't connect at all (it doesn't have the private key).

use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider},
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
    server::danger::{ClientCertVerified, ClientCertVerifier},
    DigitallySignedStruct, DistinguishedName, SignatureScheme,
};
use std::{io, net::SocketAddr, path::Path, sync::Arc, time::Duration};
use tokio::{net::TcpStream, time::timeout};
use tokio_rustls::{client, server, TlsAcceptor, TlsConnector};

pub type ClientStream = client::TlsStream<TcpStream>;
pub type ServerStream = server::TlsStream<TcpStream>;

/// This device's certificate and key.
pub struct Identity {
    cert: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
}

impl Identity {
    /// The one kept in `dir`, or a new one saved there (missing or unreadable: pairings must be redone).
    pub fn load_or_create(dir: &Path) -> Self {
        let (cert_path, key_path) = (dir.join("identity.crt"), dir.join("identity.key"));
        if let (Ok(cert), Ok(key)) = (std::fs::read(&cert_path), std::fs::read(&key_path)) {
            return Self { cert: cert.into(), key: key.into() };
        }
        let id = Self::generate();
        let _ = std::fs::create_dir_all(dir);
        if let Err(e) = std::fs::write(&cert_path, &id.cert).and_then(|()| std::fs::write(&key_path, id.key.secret_pkcs8_der())) {
            eprintln!("saving identity failed: {e}");
        }
        id
    }

    pub fn generate() -> Self {
        let c = rcgen::generate_simple_self_signed(vec!["loka".into()]).expect("self-signed certificate");
        Self { cert: c.cert.der().clone(), key: PrivatePkcs8KeyDer::from(c.key_pair.serialize_der()) }
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.cert)
    }
}

/// Hex SHA-256 of a certificate.
pub fn fingerprint(cert: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, cert).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// The code both screens show while pairing `a` and `b` (order doesn't matter): "123 456".
pub fn pairing_code(a: &str, b: &str) -> String {
    let (x, y) = if a < b { (a, b) } else { (b, a) };
    let d = ring::digest::digest(&ring::digest::SHA256, format!("{x}{y}").as_bytes());
    let n = u32::from_be_bytes(d.as_ref()[..4].try_into().unwrap()) % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
}

/// Accepts any certificate during the handshake (signatures are still checked, so the peer proves it
/// holds the key); who it is gets decided by its fingerprint afterwards.
#[derive(Debug)]
struct ByFingerprint(Arc<CryptoProvider>);

impl ServerCertVerifier for ByFingerprint {
    fn verify_server_cert(&self, _: &CertificateDer, _: &[CertificateDer], _: &ServerName, _: &[u8], _: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

impl ClientCertVerifier for ByFingerprint {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn verify_client_cert(&self, _: &CertificateDer, _: &[CertificateDer], _: UnixTime) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Both ends of every link, for one identity.
pub struct Tls {
    pub me: String,
    connector: TlsConnector,
    acceptor: TlsAcceptor,
}

impl Tls {
    pub fn new(id: &Identity) -> Self {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = Arc::new(ByFingerprint(provider.clone()));
        let key = || PrivateKeyDer::Pkcs8(id.key.clone_key());
        let client = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("TLS 1.3")
            .dangerous()
            .with_custom_certificate_verifier(verifier.clone())
            .with_client_auth_cert(vec![id.cert.clone()], key())
            .expect("own certificate");
        let mut server = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("TLS 1.3")
            .with_client_cert_verifier(verifier)
            .with_single_cert(vec![id.cert.clone()], key())
            .expect("own certificate");
        // no resumption (it would skip the certificate we pin), and a sender that never reads them closing
        // with tickets unread resets the link and loses the tail of a file
        server.send_tls13_tickets = 0;
        Self { me: id.fingerprint(), connector: TlsConnector::from(Arc::new(client)), acceptor: TlsAcceptor::from(Arc::new(server)) }
    }

    /// Connect to `to`; with `expect`, only if the device there is that fingerprint.
    pub async fn connect(&self, to: SocketAddr, expect: Option<&str>) -> io::Result<(ClientStream, String)> {
        let tcp = timeout(Duration::from_secs(2), TcpStream::connect(to)).await??;
        tcp.set_nodelay(true)?;
        let name = ServerName::try_from("loka").expect("static name");
        let s = timeout(Duration::from_secs(5), self.connector.connect(name, tcp)).await??;
        let fp = s.get_ref().1.peer_certificates().and_then(|c| c.first()).map(|c| fingerprint(c)).unwrap_or_default();
        if expect.is_some_and(|e| e != fp) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "another device answered at that address"));
        }
        Ok((s, fp))
    }

    /// TLS on an accepted connection: the stream and the connecting device's fingerprint.
    pub async fn accept(&self, tcp: TcpStream) -> io::Result<(ServerStream, String)> {
        let _ = tcp.set_nodelay(true);
        let s = timeout(Duration::from_secs(5), self.acceptor.accept(tcp)).await??;
        let fp = s.get_ref().1.peer_certificates().and_then(|c| c.first()).map(|c| fingerprint(c)).unwrap_or_default();
        Ok((s, fp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn both_ends_learn_who_the_other_is_and_impostors_are_refused() {
        let (a, b, c) = (Tls::new(&Identity::generate()), Arc::new(Tls::new(&Identity::generate())), Tls::new(&Identity::generate()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = b.clone();
        tokio::spawn(async move {
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                if let Ok((mut s, who)) = srv.accept(tcp).await {
                    s.write_all(who.as_bytes()).await.unwrap();
                    s.shutdown().await.unwrap();
                }
            }
        });
        let (mut s, fp) = a.connect(addr, Some(&b.me)).await.unwrap();
        assert_eq!(fp, b.me);
        let mut seen = String::new();
        s.read_to_string(&mut seen).await.unwrap();
        assert_eq!(seen, a.me, "the server sees the client's own certificate");
        // b is paired as "the device at addr": a connection expecting c there fails
        assert_eq!(a.connect(addr, Some(&c.me)).await.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(pairing_code(&a.me, &b.me), pairing_code(&b.me, &a.me));
        assert_ne!(pairing_code(&a.me, &b.me), pairing_code(&a.me, &c.me));
        assert_eq!(pairing_code(&a.me, &b.me).len(), 7);
    }

    #[test]
    fn identity_survives_restart() {
        let dir = std::env::temp_dir().join(format!("loka-id-{}", uuid::Uuid::new_v4()));
        let first = Identity::load_or_create(&dir).fingerprint();
        assert_eq!(Identity::load_or_create(&dir).fingerprint(), first);
        assert_eq!(first.len(), 64);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
