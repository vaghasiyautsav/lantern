//! QUIC sessions with identity-pinned, self-signed Ed25519 certificates.
//!
//! DESIGN.md §2.3, §3.3. There is no CA: verification asks one question —
//! does the certificate's Ed25519 key equal the identity we expect? Mutual:
//! servers require client certificates and the same check runs both ways
//! (the dialer pins the exact expected key; the acceptor requires a
//! well-formed Ed25519 cert and the core layer matches it to the Hello).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::pkcs8::EncodePrivateKey;
use lantern_crypto::Identity;
use quinn::crypto::rustls::QuicClientConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tracing::debug;

pub const ALPN: &[u8] = b"wisp/1";

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("tls setup: {0}")]
    Tls(String),
    #[error("quic: {0}")]
    Quic(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("peer presented no certificate")]
    NoPeerCert,
    #[error("peer certificate is not Ed25519")]
    NotEd25519,
}

/// LAN-tuned QUIC transport parameters. A vanished peer (kill -9, cable
/// pull, sleep) is detected in ~8 s instead of quinn's 30 s default —
/// on a LAN, a stalled sender for half a minute reads as "broken app".
/// Keepalives ride only on connections with streams open, so idle
/// conversations still quiesce.
fn lan_transport_config() -> Arc<quinn::TransportConfig> {
    let mut tc = quinn::TransportConfig::default();
    tc.max_idle_timeout(Some(
        Duration::from_secs(8).try_into().expect("valid idle timeout"),
    ));
    tc.keep_alive_interval(Some(Duration::from_secs(2)));
    Arc::new(tc)
}

/// Generate a self-signed X.509 cert whose SPKI is the device's Ed25519
/// identity key.
pub fn identity_cert(
    identity: &Identity,
) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>), TransportError> {
    let pkcs8 = identity
        .signing_key()
        .to_pkcs8_der()
        .map_err(|e| TransportError::Tls(e.to_string()))?;
    let key_pair = rcgen::KeyPair::try_from(pkcs8.as_bytes())
        .map_err(|e| TransportError::Tls(e.to_string()))?;
    let mut params = rcgen::CertificateParams::new(vec!["lantern".into()])
        .map_err(|e| TransportError::Tls(e.to_string()))?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| TransportError::Tls(e.to_string()))?;
    let key_der = PrivateKeyDer::Pkcs8(pkcs8.as_bytes().to_vec().into());
    Ok((cert.der().clone(), key_der))
}

/// Extract the raw Ed25519 public key from a certificate's SPKI.
pub fn cert_identity(cert: &CertificateDer<'_>) -> Result<[u8; 32], TransportError> {
    let (_, parsed) = x509_parser::parse_x509_certificate(cert.as_ref())
        .map_err(|e| TransportError::Tls(e.to_string()))?;
    let spki = parsed.tbs_certificate.subject_pki;
    // Ed25519 OID: 1.3.101.112
    if spki.algorithm.algorithm.to_id_string() != "1.3.101.112" {
        return Err(TransportError::NotEd25519);
    }
    let key = spki.subject_public_key.data.as_ref();
    key.try_into().map_err(|_| TransportError::NotEd25519)
}

/// Pull the peer's identity key out of an established connection.
pub fn peer_identity(conn: &quinn::Connection) -> Result<[u8; 32], TransportError> {
    let certs = conn
        .peer_identity()
        .and_then(|any| any.downcast::<Vec<CertificateDer<'static>>>().ok())
        .ok_or(TransportError::NoPeerCert)?;
    let first = certs.first().ok_or(TransportError::NoPeerCert)?;
    cert_identity(first)
}

/// Client-side verifier: pins one exact identity.
#[derive(Debug)]
struct PinnedServerVerifier {
    expected: [u8; 32],
}

impl rustls::client::danger::ServerCertVerifier for PinnedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let id = cert_identity(end_entity)
            .map_err(|_| rustls::Error::General("bad peer certificate".into()))?;
        if id == self.expected {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("identity pin mismatch".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

/// Server-side verifier: any well-formed Ed25519 client cert is admitted at
/// the TLS layer; the core layer matches its key against the Hello frame.
#[derive(Debug)]
struct AnyEd25519ClientVerifier;

impl rustls::server::danger::ClientCertVerifier for AnyEd25519ClientVerifier {
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        cert_identity(end_entity)
            .map_err(|_| rustls::Error::General("client cert is not Ed25519".into()))?;
        Ok(rustls::server::danger::ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

/// A QUIC endpoint bound to one identity: accepts inbound sessions and dials
/// outbound ones.
pub struct Transport {
    endpoint: quinn::Endpoint,
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
}

impl Transport {
    /// Bind on `port` (0 = ephemeral).
    pub fn bind(identity: &Identity, port: u16) -> Result<Self, TransportError> {
        // Install the ring provider once; subsequent calls are a no-op error
        // we can ignore.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let (cert, key) = identity_cert(identity)?;

        let mut server_tls = rustls::ServerConfig::builder()
            .with_client_cert_verifier(Arc::new(AnyEd25519ClientVerifier))
            .with_single_cert(
                vec![cert.clone()],
                key.clone_key(),
            )
            .map_err(|e| TransportError::Tls(e.to_string()))?;
        server_tls.alpn_protocols = vec![ALPN.to_vec()];

        let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(server_tls)
                .map_err(|e| TransportError::Tls(e.to_string()))?,
        ));
        server_config.transport_config(lan_transport_config());

        let addr: SocketAddr = (std::net::Ipv4Addr::UNSPECIFIED, port).into();
        let endpoint = quinn::Endpoint::server(server_config, addr)
            .map_err(|e| TransportError::Quic(e.to_string()))?;

        Ok(Self { endpoint, cert, key })
    }

    pub fn local_port(&self) -> u16 {
        self.endpoint.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    /// Dial a peer whose identity we already know from its beacon. The TLS
    /// layer refuses to complete unless the peer proves that exact key.
    pub async fn connect(
        &self,
        addr: SocketAddr,
        expected_id: [u8; 32],
    ) -> Result<quinn::Connection, TransportError> {
        let mut client_tls = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedServerVerifier {
                expected: expected_id,
            }))
            .with_client_auth_cert(vec![self.cert.clone()], self.key.clone_key())
            .map_err(|e| TransportError::Tls(e.to_string()))?;
        client_tls.alpn_protocols = vec![ALPN.to_vec()];

        let mut client_config = quinn::ClientConfig::new(Arc::new(
            QuicClientConfig::try_from(client_tls)
                .map_err(|e| TransportError::Tls(e.to_string()))?,
        ));
        client_config.transport_config(lan_transport_config());

        let connecting = self
            .endpoint
            .connect_with(client_config, addr, "lantern")
            .map_err(|e| TransportError::Quic(e.to_string()))?;
        let conn = connecting
            .await
            .map_err(|e| TransportError::Quic(e.to_string()))?;
        debug!("connected to {addr}");
        Ok(conn)
    }

    /// Accept the next inbound connection.
    pub async fn accept(&self) -> Option<quinn::Connection> {
        loop {
            let incoming = self.endpoint.accept().await?;
            match incoming.await {
                Ok(conn) => return Some(conn),
                Err(e) => {
                    debug!("inbound handshake failed: {e}");
                    continue;
                }
            }
        }
    }
}
