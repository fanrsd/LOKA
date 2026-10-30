//! Every link (input sessions, pairing, files, services) is a stream of a QUIC connection (TLS 1.3, ALPN
//! `loka/2`) with a self-signed certificate per device. A device *is* its certificate: the fingerprint
//! (SHA-256 of the certificate) is what discovery announces, what pairing stores, and what each side
//! checks on every connection. Pairing shows a 6-digit code from both fingerprints and a nonce from each
//! side (the requester's committed before it sees the other's), so a device in the middle can't grind
//! certificates until both screens match; after pairing an impostor can't connect at all (it doesn't
//! have the private key). Keep-alives make a silently dead peer (Wi-Fi drop, sleep, crash) fail every
//! stream within `IDLE`, as `io::ErrorKind::TimedOut`.

use quinn::{
    crypto::rustls::{QuicClientConfig, QuicServerConfig},
    ClientConfig, Connection, Endpoint, ReadError, RecvStream, SendStream, ServerConfig, StoppedError, TransportConfig, VarInt, WriteError,
};
use ring::rand::SecureRandom;
use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider},
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
    server::danger::{ClientCertVerified, ClientCertVerifier},
    DigitallySignedStruct, DistinguishedName, SignatureScheme,
};
use std::{
    collections::HashMap,
    future::Future,
    io,
    net::SocketAddr,
    path::Path,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{ready, Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::mpsc,
};

const ALPN: &[u8] = b"loka/2";
/// A peer that sent nothing (not even a keep-alive) for this long is gone.
const IDLE: Duration = Duration::from_secs(3);
const KEEP_ALIVE: Duration = Duration::from_secs(1);

type Acked = Pin<Box<dyn Future<Output = Result<Option<VarInt>, StoppedError>> + Send + Sync>>;

/// One bidirectional QUIC stream. `shutdown` finishes it and waits until the peer has every byte, so
/// nothing is lost when the caller then drops the stream (and with it maybe the last connection handle).
pub struct Stream {
    send: SendStream,
    recv: RecvStream,
    acked: Option<Acked>,
}

impl Stream {
    fn new((send, recv): (SendStream, RecvStream)) -> Self {
        Self { send, recv, acked: None }
    }
}

impl AsyncRead for Stream {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        self.recv.poll_read_buf(cx, buf).map_err(|e| match e {
            ReadError::ConnectionLost(c) => c.into(), // idle timeout → TimedOut
            e => e.into(),
        })
    }
}

fn write_err(e: WriteError) -> io::Error {
    match e {
        WriteError::ConnectionLost(c) => c.into(),
        e => e.into(),
    }
}

impl AsyncWrite for Stream {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        AsyncWrite::poll_write(Pin::new(&mut self.send), cx, buf).map_err(|e| match e.get_ref().and_then(|i| i.downcast_ref::<WriteError>()) {
            Some(w) => write_err(w.clone()),
            None => e,
        })
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let Stream { send, acked, .. } = &mut *self;
        let acked = acked.get_or_insert_with(|| {
            let _ = send.finish();
            Box::pin(send.stopped())
        });
        // stopped by the peer (it read all it wanted) is done too
        Poll::Ready(ready!(acked.as_mut().poll(cx)).map(|_| ()).map_err(|e| write_err(e.into())))
    }
}

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

/// The code both screens show while `requester` pairs with `answerer`, from both fingerprints and both
/// pairing nonces: "123 456".
pub fn pairing_code(requester: &str, answerer: &str, n_requester: &[u8; 32], n_answerer: &[u8; 32]) -> String {
    let mut c = ring::digest::Context::new(&ring::digest::SHA256);
    for part in [requester.as_bytes(), answerer.as_bytes(), n_requester, n_answerer] {
        c.update(part);
    }
    let n = u32::from_be_bytes(c.finish().as_ref()[..4].try_into().unwrap()) % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
}

/// A fresh pairing nonce.
pub fn nonce() -> [u8; 32] {
    let mut n = [0; 32];
    ring::rand::SystemRandom::new().fill(&mut n).expect("system random");
    n
}

/// The requester's commitment to its nonce, sent before it sees the other side's.
pub fn commit(nonce: &[u8; 32]) -> [u8; 32] {
    ring::digest::digest(&ring::digest::SHA256, nonce).as_ref().try_into().unwrap()
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
    client: ClientConfig,
    server: ServerConfig,
    /// made on first connect: `new` may run outside the async runtime
    endpoint: Mutex<Option<Endpoint>>,
    /// one connection per remote address (and the fingerprint it proved), reused by every stream to it
    links: Mutex<HashMap<SocketAddr, (Connection, String)>>,
}

/// Fingerprint of the certificate the other end of `c` proved it holds.
fn peer_fp(c: &Connection) -> String {
    let certs = c.peer_identity().and_then(|i| i.downcast::<Vec<CertificateDer<'static>>>().ok());
    certs.and_then(|c| c.first().map(|c| fingerprint(c))).unwrap_or_default()
}

impl Tls {
    pub fn new(id: &Identity) -> Self {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = Arc::new(ByFingerprint(provider.clone()));
        let key = || PrivateKeyDer::Pkcs8(id.key.clone_key());
        let mut client = rustls::ClientConfig::builder_with_provider(provider.clone())
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
        // no resumption: it would skip the certificate we pin
        server.send_tls13_tickets = 0;
        client.alpn_protocols = vec![ALPN.to_vec()];
        server.alpn_protocols = vec![ALPN.to_vec()];
        let mut transport = TransportConfig::default();
        transport.keep_alive_interval(Some(KEEP_ALIVE)).max_idle_timeout(Some(IDLE.try_into().expect("idle timeout")));
        let transport = Arc::new(transport);
        let mut client = ClientConfig::new(Arc::new(QuicClientConfig::try_from(Arc::new(client)).expect("QUIC cipher suite")));
        client.transport_config(transport.clone());
        let mut server = ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(Arc::new(server)).expect("QUIC cipher suite")));
        server.transport_config(transport);
        Self { me: id.fingerprint(), client, server, endpoint: Mutex::new(None), links: Mutex::new(HashMap::new()) }
    }

    fn endpoint(&self) -> io::Result<Endpoint> {
        let mut e = self.endpoint.lock().unwrap();
        if let Some(e) = &*e {
            return Ok(e.clone());
        }
        let mut ep = Endpoint::client(([0, 0, 0, 0], 0).into())?;
        ep.set_default_client_config(self.client.clone());
        Ok(e.insert(ep).clone())
    }

    /// A new stream to `to`; with `expect`, only if the device there is that fingerprint. The peer sees
    /// the stream once something is written on it.
    pub async fn connect(&self, to: SocketAddr, expect: Option<&str>) -> io::Result<(Stream, String)> {
        let cached = self.links.lock().unwrap().get(&to).filter(|(c, fp)| c.close_reason().is_none() && expect.is_none_or(|e| e == fp)).cloned();
        let (conn, fp) = match cached {
            Some(link) => link,
            None => {
                // a host that never answers fails the handshake after IDLE
                let conn = self.endpoint()?.connect(to, "loka").map_err(io::Error::other)?.await?;
                let fp = peer_fp(&conn);
                if expect.is_some_and(|e| e != fp) {
                    conn.close(0u32.into(), b"");
                    return Err(io::Error::new(io::ErrorKind::PermissionDenied, "another device answered at that address"));
                }
                self.links.lock().unwrap().insert(to, (conn.clone(), fp.clone()));
                (conn, fp)
            }
        };
        Ok((Stream::new(conn.open_bi().await?), fp))
    }

    /// Accept streams on UDP `port`. Handshakes and each connection's streams are handled in their own
    /// tasks, so a slow peer never holds up the others.
    pub fn listen(&self, port: u16) -> io::Result<Listener> {
        let ep = Endpoint::server(self.server.clone(), ([0, 0, 0, 0], port).into())?;
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(async move {
            while let Some(incoming) = ep.accept().await {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let Ok(conn) = incoming.await else { return };
                    let fp = peer_fp(&conn);
                    while let Ok(s) = conn.accept_bi().await {
                        if tx.send((Stream::new(s), fp.clone(), conn.remote_address())).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Ok(Listener(rx))
    }
}

pub struct Listener(mpsc::Receiver<(Stream, String, SocketAddr)>);

impl Listener {
    /// The next stream a device opened: the stream, that device's fingerprint and its address.
    pub async fn accept(&mut self) -> io::Result<(Stream, String, SocketAddr)> {
        self.0.recv().await.ok_or_else(|| io::Error::other("listener stopped"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn both_ends_learn_who_the_other_is_and_impostors_are_refused() {
        let (a, b, c) = (Tls::new(&Identity::generate()), Tls::new(&Identity::generate()), Tls::new(&Identity::generate()));
        let port = 53387;
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();
        let mut listener = b.listen(port).unwrap();
        let (got_tx, mut got) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let (mut s, who, _) = listener.accept().await.unwrap();
                let got_tx = got_tx.clone();
                tokio::spawn(async move {
                    let mut all = Vec::new();
                    s.read_to_end(&mut all).await.unwrap();
                    let _ = got_tx.send(all.len());
                    // the connecting side may be gone already (last case below)
                    let _ = s.write_all(who.as_bytes()).await;
                    let _ = s.shutdown().await;
                });
            }
        });
        let (mut s, fp) = a.connect(addr, Some(&b.me)).await.unwrap();
        assert_eq!(fp, b.me);
        s.write_all(b"hi").await.unwrap();
        s.shutdown().await.unwrap();
        let mut seen = String::new();
        s.read_to_string(&mut seen).await.unwrap();
        assert_eq!(seen, a.me, "the server sees the client's own certificate");

        // the next stream rides the same connection
        let id = a.links.lock().unwrap()[&addr].0.stable_id();
        let (mut s, _) = a.connect(addr, Some(&b.me)).await.unwrap();
        s.write_all(b"again").await.unwrap();
        s.shutdown().await.unwrap();
        assert_eq!(a.links.lock().unwrap()[&addr].0.stable_id(), id);
        assert_eq!(got.recv().await, Some(2));
        assert_eq!(got.recv().await, Some(5));

        // b is paired as "the device at addr": a connection expecting c there fails
        assert_eq!(a.connect(addr, Some(&c.me)).await.map(|_| ()).unwrap_err().kind(), io::ErrorKind::PermissionDenied);

        // a sender that drops everything (its last connection handle too) right after shutdown loses nothing
        let big = vec![7u8; 3 << 20];
        {
            let d = Tls::new(&Identity::generate());
            let (mut s, _) = d.connect(addr, None).await.unwrap();
            s.write_all(&big).await.unwrap();
            s.shutdown().await.unwrap();
        }
        assert_eq!(got.recv().await, Some(big.len()));
    }

    #[test]
    fn pairing_code_needs_both_nonces_and_knows_who_asked() {
        let (a, b) = (Identity::generate().fingerprint(), Identity::generate().fingerprint());
        let (na, nb) = (nonce(), nonce());
        let code = pairing_code(&a, &b, &na, &nb);
        assert_eq!(code.len(), 7);
        assert_eq!(code, pairing_code(&a, &b, &na, &nb));
        assert_ne!(code, pairing_code(&a, &b, &na, &nonce()));
        assert_ne!(code, pairing_code(&b, &a, &na, &nb));
        assert_eq!(commit(&na), commit(&na));
        assert_ne!(commit(&na), commit(&nb));
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
