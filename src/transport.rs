//! Workload-authenticated local sockets with bounded ingress and signed messages.
use crate::{
    audit::Hash,
    identity::{WorkloadId, workload_public_key},
    security::AuthenticatedSubject,
};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, VerifyingKey};
use http_body_util::{BodyExt, Full};
use hyper::{
    Request, Response,
    body::{Bytes, Incoming},
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use rustls::{
    RootCertStore, ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    server::WebPkiClientVerifier,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::{
    net::UnixListener,
    sync::{Semaphore, watch},
    task::JoinSet,
};
use tokio_rustls::TlsAcceptor;
use x509_parser::{extensions::GeneralName, prelude::*};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub request_id: String,
    pub issued_ms: u64,
    pub path: String,
    pub payload: String,
    pub signature: String,
}
impl Message {
    pub fn signing_bytes(&self) -> Result<Vec<u8>> {
        ensure!(
            !self.request_id.is_empty()
                && self.request_id.len() <= 128
                && !self.request_id.contains('\n'),
            "invalid request ID"
        );
        ensure!(
            self.path.starts_with('/') && self.path.len() <= 128 && !self.path.contains('\n'),
            "invalid signed operation path"
        );
        Ok(format!(
            "solos-swarm-request-v1\n{}\n{}\n{}\n{}",
            self.request_id, self.issued_ms, self.path, self.payload
        )
        .into_bytes())
    }
    pub fn verify(&self, key: &Hash, now_ms: u64, max_bytes: usize) -> Result<Value> {
        ensure!(
            self.payload.len() <= max_bytes
                && self.issued_ms <= now_ms + 5000
                && self.issued_ms + 60_000 >= now_ms,
            "request stale or oversized"
        );
        let signature = Signature::from_slice(&STANDARD.decode(&self.signature)?)?;
        VerifyingKey::from_bytes(key)?.verify_strict(&self.signing_bytes()?, &signature)?;
        Ok(serde_json::from_str(&self.payload)?)
    }
}
#[derive(Clone)]
pub struct Peer {
    pub subject: AuthenticatedSubject,
    pub workload: WorkloadId,
    pub public_key: Hash,
    pub certificate_der: Vec<u8>,
}
pub fn authenticate(leaf: &[u8], ca: &[u8], now_unix: u64) -> Result<Peer> {
    let (_, certificate) =
        parse_x509_certificate(leaf).map_err(|_| anyhow::anyhow!("malformed certificate"))?;
    let san = certificate
        .subject_alternative_name()?
        .context("workload identity SAN missing")?;
    let uris: Vec<_> = san
        .value
        .general_names
        .iter()
        .filter_map(|name| match name {
            GeneralName::URI(uri) => Some(*uri),
            _ => None,
        })
        .collect();
    ensure!(uris.len() == 1, "ambiguous workload identity");
    let workload = WorkloadId::parse(uris[0])?;
    Ok(Peer {
        subject: AuthenticatedSubject::workload(leaf, ca, workload.clone(), now_unix)?,
        workload,
        public_key: workload_public_key(leaf)?,
        certificate_der: leaf.into(),
    })
}
pub type HandlerFuture = Pin<Box<dyn Future<Output = Result<Value>> + Send>>;
pub trait Handler: Send + Sync {
    fn handle(&self, peer: Peer, path: String, message: Message, payload: Value) -> HandlerFuture;
}
pub struct CertificateResolver {
    current: RwLock<Arc<rustls::sign::CertifiedKey>>,
}
impl std::fmt::Debug for CertificateResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CertificateResolver([REDACTED])")
    }
}
impl CertificateResolver {
    pub fn new(certificate: Vec<u8>, private_key: Vec<u8>) -> Result<Self> {
        Ok(Self {
            current: RwLock::new(Self::certified(certificate, private_key)?),
        })
    }
    fn certified(
        certificate: Vec<u8>,
        private_key: Vec<u8>,
    ) -> Result<Arc<rustls::sign::CertifiedKey>> {
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(private_key));
        Ok(Arc::new(rustls::sign::CertifiedKey::from_der(
            vec![CertificateDer::from(certificate)],
            key,
            &rustls::crypto::ring::default_provider(),
        )?))
    }
    pub fn update(&self, certificate: Vec<u8>, private_key: Vec<u8>) -> Result<()> {
        let certified = Self::certified(certificate, private_key)?;
        *self
            .current
            .write()
            .map_err(|_| anyhow::anyhow!("TLS identity lock failed"))? = certified;
        Ok(())
    }
}
impl rustls::server::ResolvesServerCert for CertificateResolver {
    fn resolve(
        &self,
        _hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        self.current.read().ok().map(|value| value.clone())
    }
}
#[derive(Clone)]
pub struct SocketConfig {
    pub path: PathBuf,
    pub certificate_der: Vec<u8>,
    pub private_key_der: Vec<u8>,
    pub ca_der: Vec<u8>,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    pub connections: usize,
    pub request_timeout_ms: u64,
    pub resolver: Option<Arc<CertificateResolver>>,
}
pub async fn serve(
    config: SocketConfig,
    handler: Arc<dyn Handler>,
    mut stop: watch::Receiver<bool>,
) -> Result<()> {
    ensure!(
        config.connections > 0
            && config.max_request_bytes > 0
            && config.max_response_bytes > 0
            && config.request_timeout_ms > 0,
        "socket bounds missing"
    );
    ensure!(
        !config.path.exists(),
        "socket already exists; reconcile its owning run before launch"
    );
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(config.ca_der.clone()))?;
    let verifier = WebPkiClientVerifier::builder(Arc::new(roots)).build()?;
    let builder = ServerConfig::builder().with_client_cert_verifier(verifier);
    let mut tls = if let Some(resolver) = config.resolver.clone() {
        builder.with_cert_resolver(resolver)
    } else {
        builder.with_single_cert(
            vec![CertificateDer::from(config.certificate_der.clone())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(config.private_key_der.clone())),
        )?
    };
    tls.max_early_data_size = 0;
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(tls));
    let listener = UnixListener::bind(&config.path)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&config.path, std::fs::Permissions::from_mode(0o600))?;
    let slots = Arc::new(Semaphore::new(config.connections));
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (socket, _) = accepted?;
                let Ok(slot) = slots.clone().try_acquire_owned() else { drop(socket); continue; };
                let tls = acceptor.clone(); let handler = handler.clone(); let config = config.clone();
                tasks.spawn(async move {
                    let _slot = slot;
                    let stream = tokio::time::timeout(Duration::from_secs(5), tls.accept(socket)).await??;
                    let leaf = stream.get_ref().1.peer_certificates().and_then(|certs| certs.first()).context("client certificate missing")?.as_ref().to_vec();
                    authenticate(&leaf, &config.ca_der, crate::cluster::now_ms() / 1000)?;
                    let service = service_fn(move |request: Request<Incoming>| {
                        let leaf = leaf.clone(); let config = config.clone(); let handler = handler.clone();
                        async move {
                            let response = tokio::time::timeout(Duration::from_millis(config.request_timeout_ms), process(request, &leaf, &config, handler)).await;
                            let (status, value) = match response {
                                Ok(Ok(value)) => (200, value), Ok(Err(error)) => (403, rejection(&error)),
                                Err(_) => (504, serde_json::json!({"error":"broker deadline reached; inspect effect before retry"})),
                            };
                            let mut bytes = serde_json::to_vec(&value).unwrap_or_else(|_| b"{\"error\":\"encoding failed\"}".to_vec());
                            let status = if bytes.len() > config.max_response_bytes { bytes = b"{\"error\":\"response byte ceiling reached\"}".to_vec(); 413 } else { status };
                            Ok::<_, std::convert::Infallible>(Response::builder().status(status).header("Content-Type", "application/json").body(Full::new(Bytes::from(bytes))).expect("valid response"))
                        }
                    });
                    hyper::server::conn::http1::Builder::new().max_buf_size(16_384).header_read_timeout(Duration::from_secs(5))
                        .timer(hyper_util::rt::TokioTimer::new()).serve_connection(TokioIo::new(stream), service).await?;
                    Ok::<(), anyhow::Error>(())
                });
            },
            _ = stop.changed() => { if *stop.borrow() { break; } },
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    drop(listener);
    remove_socket(&config.path)?;
    Ok(())
}
fn rejection(error: &anyhow::Error) -> Value {
    let mut value = serde_json::json!({"error":error.to_string()});
    if error
        .downcast_ref::<crate::kernel::StateCapacityError>()
        .is_some()
    {
        value["code"] = Value::String("state_capacity".into());
    }
    value
}
fn remove_socket(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
async fn process(
    request: Request<Incoming>,
    leaf: &[u8],
    config: &SocketConfig,
    handler: Arc<dyn Handler>,
) -> Result<Value> {
    ensure!(
        request.method() == hyper::Method::POST && request.uri().query().is_none(),
        "only signed POST messages are accepted"
    );
    let path = request.uri().path().to_owned();
    ensure!(path.len() <= 128, "path exceeds ingress ceiling");
    if let Some(size) = request.headers().get("Content-Length") {
        ensure!(
            size.to_str()?.parse::<usize>()? <= config.max_request_bytes,
            "request byte ceiling reached"
        );
    }
    let mut body = request.into_body();
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame?;
        if let Some(data) = frame.data_ref() {
            ensure!(
                bytes.len() + data.len() <= config.max_request_bytes,
                "request byte ceiling reached"
            );
            bytes.extend_from_slice(data);
        }
    }
    let message: Message = serde_json::from_slice(&bytes)?;
    ensure!(
        message.path == path,
        "signed operation differs from requested path"
    );
    let peer = authenticate(leaf, &config.ca_der, crate::cluster::now_ms() / 1000)?;
    let payload = message.verify(
        &peer.public_key,
        crate::cluster::now_ms(),
        config.max_request_bytes,
    )?;
    handler.handle(peer, path, message, payload).await
}
#[cfg(test)]
mod tests {
    #[test]
    fn only_typed_capacity_rejections_carry_a_retriable_code() {
        let typed: anyhow::Error = crate::kernel::StateCapacityError.into();
        assert_eq!(super::rejection(&typed)["code"], "state_capacity");
        let text_only = anyhow::anyhow!(typed.to_string());
        assert!(super::rejection(&text_only).get("code").is_none());
    }
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    #[test]
    fn signed_payload_binds_identity_id_time_and_exact_bytes() {
        let key = SigningKey::from_bytes(&[11; 32]);
        let mut message = Message {
            request_id: "stable-1".into(),
            issued_ms: 10_000,
            path: "/tool/board_post".into(),
            payload: "{\"n\":1}".into(),
            signature: String::new(),
        };
        message.signature = STANDARD.encode(key.sign(&message.signing_bytes().unwrap()).to_bytes());
        assert_eq!(
            message
                .verify(&key.verifying_key().to_bytes(), 10_001, 100)
                .unwrap()["n"],
            1
        );
        let mut changed = message.clone();
        changed.payload.push(' ');
        assert!(
            changed
                .verify(&key.verifying_key().to_bytes(), 10_001, 100)
                .is_err()
        );
        let mut changed = message.clone();
        changed.path = "/tool/submit_attempt".into();
        assert!(
            changed
                .verify(&key.verifying_key().to_bytes(), 10_001, 100)
                .is_err()
        );
        assert!(message.verify(&[0; 32], 10_001, 100).is_err());
        assert!(
            message
                .verify(&key.verifying_key().to_bytes(), 100_001, 100)
                .is_err()
        );
        assert!(
            message
                .verify(&key.verifying_key().to_bytes(), 10_001, 2)
                .is_err()
        );
    }
}
