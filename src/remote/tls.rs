//! Mutually authenticated TLS between the agent and its nodes. Each side
//! has a self-signed certificate made at setup, and each pins the other's
//! by its SHA-256 fingerprint from config: no CA, no names, no expiry. A
//! handshake still proves the peer holds the pinned certificate's key.

use anyhow::{Context, Result, bail, ensure};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, DistinguishedName, ServerConfig,
    SignatureScheme,
};
use serde::{Deserialize, Deserializer};
use sha2::{Digest, Sha256};
use std::fmt;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

/// The server name a node asks for. Pinning, not the name, authenticates.
pub const AGENT_NAME: &str = "solos-swarm-agent";
pub const NODE_NAME: &str = "solos-swarm-node";
pub const ALPN: &[u8] = b"solos-node/1";
pub const CERT_FILE: &str = "cert.pem";
pub const KEY_FILE: &str = "key.pem";

/// SHA-256 of a certificate's DER, written `sha256:` and 64 hex digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    pub fn of(der: &[u8]) -> Self {
        Self(Sha256::digest(der).into())
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("sha256:")?;
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl FromStr for Fingerprint {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        let hex = text
            .trim()
            .strip_prefix("sha256:")
            .context("a fingerprint starts with sha256:")?;
        ensure!(
            hex.len() == 64 && hex.is_ascii(),
            "a fingerprint has 64 hex digits after sha256:"
        );
        let mut bytes = [0u8; 32];
        for (at, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[at * 2..at * 2 + 2], 16)
                .context("a fingerprint has 64 hex digits after sha256:")?;
        }
        Ok(Self(bytes))
    }
}

impl<'de> Deserialize<'de> for Fingerprint {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// Write a new key and self-signed certificate in `dir`, and return the
/// certificate's fingerprint. An existing key is never replaced.
pub fn init(dir: &Path, subject: &str) -> Result<Fingerprint> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("cannot create {}", dir.display()))?;
    let made = rcgen::generate_simple_self_signed(vec![subject.to_owned()])
        .context("cannot make a certificate")?;
    let key = dir.join(KEY_FILE);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&key)
        .with_context(|| format!("cannot create {}; is there a key already?", key.display()))?;
    file.write_all(made.signing_key.serialize_pem().as_bytes())?;
    file.sync_all()?;
    std::fs::write(dir.join(CERT_FILE), made.cert.pem())
        .with_context(|| format!("cannot write {}", dir.join(CERT_FILE).display()))?;
    Ok(Fingerprint::of(made.cert.der()))
}

/// One side's certificate and key.
pub struct Identity {
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    pub fingerprint: Fingerprint,
}

impl Identity {
    /// Read `cert` and `key` as `init` wrote them. A key that anyone but
    /// its owner can read is refused.
    pub fn load(cert: &Path, key: &Path) -> Result<Self> {
        let mode = std::fs::metadata(key)
            .with_context(|| format!("cannot read {}", key.display()))?
            .permissions()
            .mode();
        ensure!(
            mode & 0o077 == 0,
            "{} is readable by others; chmod 600 it",
            key.display()
        );
        let mut certs = rustls_pemfile::certs(&mut pem(cert)?.as_slice())
            .collect::<std::io::Result<Vec<_>>>()
            .with_context(|| format!("{} is not a PEM certificate", cert.display()))?;
        ensure!(
            certs.len() == 1,
            "{} must hold one certificate",
            cert.display()
        );
        let cert = certs.remove(0);
        let Some(key) = rustls_pemfile::private_key(&mut pem(key)?.as_slice())
            .with_context(|| format!("{} is not a PEM key", key.display()))?
        else {
            bail!("{} holds no private key", key.display());
        };
        Ok(Self {
            fingerprint: Fingerprint::of(&cert),
            cert,
            key,
        })
    }
}

fn pem(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))
}

/// The paths `init` wrote in `dir`.
pub fn files(dir: &Path) -> (PathBuf, PathBuf) {
    (dir.join(CERT_FILE), dir.join(KEY_FILE))
}

/// The agent's side: it serves `identity` and admits a client only with a
/// certificate in `allowed`.
pub fn server_config(identity: &Identity, allowed: Vec<Fingerprint>) -> Result<Arc<ServerConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(Pinned::new(allowed, &provider));
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![identity.cert.clone()], identity.key.clone_key())?;
    config.alpn_protocols = vec![ALPN.to_vec()];
    config.max_early_data_size = 0;
    Ok(Arc::new(config))
}

/// A node's side: it presents `identity` and talks only to the agent whose
/// certificate is `agent`.
pub fn client_config(identity: &Identity, agent: Fingerprint) -> Result<Arc<ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(Pinned::new(vec![agent], &provider));
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![identity.cert.clone()], identity.key.clone_key())?;
    config.alpn_protocols = vec![ALPN.to_vec()];
    config.enable_early_data = false;
    Ok(Arc::new(config))
}

pub fn agent_name() -> Result<ServerName<'static>> {
    ServerName::try_from(AGENT_NAME).context("the agent's server name")
}

/// The certificate the peer of `connection` presented.
pub fn peer(connection: &rustls::CommonState) -> Option<Fingerprint> {
    connection
        .peer_certificates()
        .and_then(|certs| certs.first())
        .map(|cert| Fingerprint::of(cert))
}

/// Accepts exactly the pinned certificates, and checks each handshake
/// signature against the presented one.
#[derive(Debug)]
struct Pinned {
    allowed: Vec<Fingerprint>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl Pinned {
    fn new(allowed: Vec<Fingerprint>, provider: &rustls::crypto::CryptoProvider) -> Self {
        Self {
            allowed,
            algorithms: provider.signature_verification_algorithms,
        }
    }

    fn check(&self, cert: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        if self.allowed.contains(&Fingerprint::of(cert)) {
            Ok(())
        } else {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|()| ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

impl ClientCertVerifier for Pinned {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|()| ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::{Fingerprint, Identity, init};

    #[test]
    fn a_fingerprint_reads_back_what_it_writes_and_refuses_anything_else() {
        let fingerprint = Fingerprint::of(b"certificate");
        let text = fingerprint.to_string();
        assert!(text.starts_with("sha256:") && text.len() == 71, "{text}");
        assert_eq!(text.parse::<Fingerprint>().unwrap(), fingerprint);
        assert_eq!(
            text.to_uppercase()
                .replace("SHA256:", "sha256:")
                .parse::<Fingerprint>()
                .unwrap(),
            fingerprint
        );
        for bad in [
            &text[7..],
            &text[..70],
            "sha256:zz",
            "md5:00",
            &format!("{text}00"),
        ] {
            assert!(bad.parse::<Fingerprint>().is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn init_writes_a_private_key_once_and_its_certificate_loads_with_that_fingerprint() {
        let dir = std::env::temp_dir().join(format!("remote-tls-{}", uuid::Uuid::new_v4()));
        let made = init(&dir, "solos-swarm-node").unwrap();
        assert!(
            init(&dir, "solos-swarm-node").is_err(),
            "a key was replaced"
        );
        let (cert, key) = super::files(&dir);
        let loaded = Identity::load(&cert, &key).unwrap();
        assert_eq!(loaded.fingerprint, made);
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        let open = Identity::load(&cert, &key).err().unwrap().to_string();
        assert!(open.contains("readable by others"), "{open}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
