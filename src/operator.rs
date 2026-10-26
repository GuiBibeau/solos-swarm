//! OIDC-authenticated operator transport. The kernel owns atomic policy/audit mutations.
use crate::{
    audit::{CryptoSigner, Hash, Signed},
    domain::GoalSpec,
    identity::{OidcConfig, VerifiedOperator, verify_operator_jwt},
    security::{
        AuthenticatedSubject, Binding, ConfirmationTier, DeploymentPolicy, GoalPosture,
        HumanApproval, IsolationTier, Role, Scope, SecurityState,
    },
};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::jwk::JwkSet;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

pub type KernelFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;
pub trait OperatorKernel: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    fn audit_authentication<'a>(
        &'a self,
        id: &'a str,
        operator: Option<&'a VerifiedOperator>,
        accepted: bool,
        reason_hash: Hash,
        signer: &'a CryptoSigner,
        now: u64,
    ) -> KernelFuture<'a, ()>;
    fn security_state(&self) -> KernelFuture<'_, SecurityState>;
    fn control_security_state<'a>(
        &'a self,
        _operator: &'a VerifiedOperator,
        _command: &'a OperatorCommand,
        _now: u64,
    ) -> KernelFuture<'a, SecurityState> {
        Box::pin(async { anyhow::bail!("verified control lookup unavailable") })
    }
    fn audit_control_authentication<'a>(
        &'a self,
        id: &'a str,
        operator: &'a VerifiedOperator,
        reason_hash: Hash,
        signer: &'a CryptoSigner,
        now: u64,
    ) -> KernelFuture<'a, ()> {
        self.audit_authentication(id, Some(operator), true, reason_hash, signer, now)
    }
    fn apply_operator<'a>(
        &'a self,
        operator: VerifiedOperator,
        request: OperatorRequest,
        signer: &'a CryptoSigner,
        now: u64,
    ) -> KernelFuture<'a, Value>;
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum OperatorCommand {
    RegisterGoal {
        spec: GoalSpec,
        posture: GoalPosture,
        approvals: Vec<Signed<HumanApproval>>,
    },
    Approve {
        action_hash: Hash,
        evidence_hash: Hash,
        grant_version: u64,
        scope: Scope,
        expiry: u64,
    },
    EngageStop {
        scope: Scope,
        reason: String,
    },
    ReleaseStop {
        scope: Scope,
        approvals: Vec<Signed<HumanApproval>>,
    },
    CreateRole {
        role: Role,
        approvals: Vec<Signed<HumanApproval>>,
    },
    BindRole {
        binding: Binding,
        approvals: Vec<Signed<HumanApproval>>,
    },
    ReadState {
        scope: Scope,
    },
    InstallPolicy {
        policy: Signed<DeploymentPolicy>,
        approvals: Vec<Signed<HumanApproval>>,
    },
    AmendGoal {
        spec: GoalSpec,
        posture: GoalPosture,
        approvals: Vec<Signed<HumanApproval>>,
    },
    ApplySwarm {
        spec: crate::placement::SwarmSpec,
    },
    ConfigureQuota {
        quota: Signed<crate::quota::QuotaConfig>,
        approvals: Vec<Signed<HumanApproval>>,
    },
    Module {
        #[serde(rename = "module")]
        command: crate::runtime::ModuleCommand,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorRequest {
    pub request_id: String,
    pub operation: OperatorCommand,
}
impl OperatorCommand {
    pub(crate) fn permission(&self) -> (&str, &str, Scope, u64, ConfirmationTier) {
        match self {
            Self::RegisterGoal { posture, .. } => (
                "register",
                "goals",
                Scope::Tenant {
                    tenant: posture.tenant.clone(),
                },
                posture.limits.total,
                ConfirmationTier::C2,
            ),
            Self::AmendGoal { posture, .. } => (
                "amend",
                "goals",
                Scope::Goal {
                    tenant: posture.tenant.clone(),
                    goal: posture.id.clone(),
                },
                posture.limits.total,
                ConfirmationTier::C3,
            ),
            Self::Approve { scope, .. } => {
                ("approve", "actions", scope.clone(), 0, ConfirmationTier::C3)
            }
            Self::EngageStop { scope, .. } => {
                ("engage", "stops", scope.clone(), 0, ConfirmationTier::C0)
            }
            Self::ReleaseStop { scope, .. } => {
                ("release", "stops", scope.clone(), 0, ConfirmationTier::C3)
            }
            Self::CreateRole { .. } => ("create", "roles", Scope::Global, 0, ConfirmationTier::C3),
            Self::BindRole { binding, .. } => (
                "create",
                "bindings",
                binding.scope.clone(),
                0,
                ConfirmationTier::C3,
            ),
            Self::ReadState { scope } => ("read", "state", scope.clone(), 0, ConfirmationTier::C0),
            Self::InstallPolicy { .. } => {
                ("amend", "policy", Scope::Global, 0, ConfirmationTier::C3)
            }
            Self::ApplySwarm { spec } => (
                "define",
                "agents",
                Scope::Goal {
                    tenant: spec.tenant.clone(),
                    goal: spec.goal_id.clone(),
                },
                0,
                ConfirmationTier::C2,
            ),
            Self::ConfigureQuota { .. } => {
                ("amend", "policy", Scope::Global, 0, ConfirmationTier::C3)
            }
            Self::Module { command } => match command {
                crate::runtime::ModuleCommand::Register { .. } => (
                    "register",
                    "modules",
                    Scope::Global,
                    0,
                    ConfirmationTier::C2,
                ),
                crate::runtime::ModuleCommand::Unload { .. } => {
                    ("unload", "modules", Scope::Global, 0, ConfirmationTier::C1)
                }
                crate::runtime::ModuleCommand::Reload { .. } => {
                    ("reload", "modules", Scope::Global, 0, ConfirmationTier::C1)
                }
                crate::runtime::ModuleCommand::RetryCleanup { .. } => (
                    "retry_cleanup",
                    "modules",
                    Scope::Global,
                    0,
                    ConfirmationTier::C1,
                ),
            },
        }
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct Challenge {
    pub nonce: String,
    pub issuer: String,
    pub audience: String,
    pub expires: u64,
}
#[derive(Clone, Debug, Serialize)]
pub struct SessionHandle {
    pub session_id: String,
    pub expires: u64,
    pub subject: String,
}
struct Session {
    nonce: String,
    token_hash: Hash,
    subject: String,
    expires: u64,
}
pub struct OperatorGateway {
    pub config: OidcConfig,
    jwks: JwkSet,
    challenges: BTreeMap<String, u64>,
    sessions: BTreeMap<String, Session>,
    control_roots: std::collections::BTreeSet<String>,
}
/// Bounded issuer metadata retrieval. The discovery origin and subsequent JWKS URL stay pinned.
pub struct JwksRefresher {
    client: reqwest::Client,
    issuer: String,
    algorithms: std::collections::BTreeSet<String>,
    jwks_uri: Option<reqwest::Url>,
}
#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    jwks_uri: String,
    id_token_signing_alg_values_supported: std::collections::BTreeSet<String>,
}
fn https_metadata_url(value: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value)?;
    ensure!(
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "OIDC metadata must use fixed authenticated HTTPS"
    );
    Ok(url)
}
fn metadata_ttl(headers: &reqwest::header::HeaderMap) -> Result<u64> {
    let mut ttl = 300;
    if let Some(value) = headers.get(reqwest::header::CACHE_CONTROL) {
        let value = value.to_str()?;
        ensure!(value.len() <= 1024, "OIDC cache metadata exceeds ceiling");
        for part in value.split(',').map(str::trim) {
            if part.eq_ignore_ascii_case("no-cache") || part.eq_ignore_ascii_case("no-store") {
                ttl = 1;
            }
            if let Some(value) = part.strip_prefix("max-age=") {
                ttl = ttl.min(value.trim_matches('"').parse::<u64>()?.max(1));
            }
        }
    }
    if let Some(age) = headers.get(reqwest::header::AGE) {
        ttl = ttl.saturating_sub(age.to_str()?.parse::<u64>()?).max(1);
    }
    Ok(ttl.min(900))
}
impl JwksRefresher {
    pub fn new(config: &OidcConfig, issuer_ca: Option<&[u8]>) -> Result<Self> {
        https_metadata_url(&config.issuer)?;
        ensure!(
            !config.algorithms.is_empty()
                && config.algorithms.iter().all(|algorithm| matches!(
                    algorithm.as_str(),
                    "RS256"
                        | "RS384"
                        | "RS512"
                        | "PS256"
                        | "PS384"
                        | "PS512"
                        | "ES256"
                        | "ES384"
                        | "EdDSA"
                )),
            "OIDC asymmetric signing algorithm allowlist missing"
        );
        let mut client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(std::time::Duration::from_secs(3))
            .timeout(std::time::Duration::from_secs(5))
            .min_tls_version(reqwest::tls::Version::TLS_1_3);
        if let Some(ca) = issuer_ca {
            ensure!(ca.len() <= 65536, "OIDC issuer CA exceeds ceiling");
            client = client.add_root_certificate(reqwest::Certificate::from_pem(ca)?);
        }
        Ok(Self {
            client: client.build()?,
            issuer: config.issuer.clone(),
            algorithms: config.algorithms.clone(),
            jwks_uri: None,
        })
    }
    async fn document<T: serde::de::DeserializeOwned>(
        &self,
        url: reqwest::Url,
    ) -> Result<(T, u64)> {
        let mut response = self.client.get(url).send().await?;
        ensure!(
            response.status().is_success(),
            "OIDC metadata endpoint rejected refresh"
        );
        if let Some(bytes) = response.content_length() {
            ensure!(bytes <= 262144, "OIDC metadata exceeds ceiling");
        }
        let ttl = metadata_ttl(response.headers())?;
        let mut bytes = vec![];
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len() + chunk.len() <= 262144,
                "OIDC metadata exceeds ceiling"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok((serde_json::from_slice(&bytes)?, ttl))
    }
    pub async fn refresh(&mut self, now: u64) -> Result<(JwkSet, u64)> {
        let discovery_url = https_metadata_url(&format!(
            "{}/.well-known/openid-configuration",
            self.issuer.trim_end_matches('/')
        ))?;
        let (discovery, discovery_ttl): (Discovery, u64) = self.document(discovery_url).await?;
        ensure!(
            discovery.issuer == self.issuer
                && self
                    .algorithms
                    .is_subset(&discovery.id_token_signing_alg_values_supported),
            "OIDC issuer/signing policy differs from pinned configuration"
        );
        let url = https_metadata_url(&discovery.jwks_uri)?;
        ensure!(
            self.jwks_uri
                .as_ref()
                .is_none_or(|previous| previous == &url),
            "OIDC JWKS endpoint changed; operator readmission required"
        );
        let (mut keys, ttl): (JwkSet, u64) = self.document(url.clone()).await?;
        ensure!(keys.keys.len() <= 64, "OIDC JWKS key count exceeds ceiling");
        keys.keys.retain(|key| {
            key.common
                .public_key_use
                .as_ref()
                .is_none_or(|usage| *usage == jsonwebtoken::jwk::PublicKeyUse::Signature)
                && key
                    .common
                    .key_algorithm
                    .as_ref()
                    .is_some_and(|algorithm| self.algorithms.contains(&algorithm.to_string()))
        });
        ensure!(
            !keys.keys.is_empty(),
            "OIDC metadata contains no allowed signing keys"
        );
        let mut ids = std::collections::BTreeSet::new();
        for key in &keys.keys {
            let id = key
                .common
                .key_id
                .as_ref()
                .context("OIDC signing key ID missing")?;
            ensure!(
                !id.is_empty() && id.len() <= 128 && ids.insert(id),
                "OIDC key ID invalid or ambiguous"
            );
            jsonwebtoken::DecodingKey::from_jwk(key)?;
        }
        self.jwks_uri = Some(url);
        Ok((
            keys,
            now.max(crate::cluster::now_ms() / 1000)
                .checked_add(ttl.min(discovery_ttl))
                .context("OIDC cache expiry overflow")?,
        ))
    }
}
pub async fn refresh_jwks(
    gateway: Arc<Mutex<OperatorGateway>>,
    mut source: JwksRefresher,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    loop {
        if *stop.borrow() {
            return Ok(());
        }
        let timestamp = now();
        let wait = match source.refresh(timestamp).await {
            Ok((keys, expiry)) => {
                let mut gateway = gateway.lock().await;
                gateway.jwks = keys;
                gateway.config.jwks_expires_unix = expiry;
                expiry.saturating_sub(timestamp).max(1) / 2 + 1
            }
            Err(_) => {
                let mut gateway = gateway.lock().await;
                gateway.config.jwks_expires_unix = 0;
                gateway.sessions.clear();
                gateway.challenges.clear();
                10
            }
        };
        tokio::select! {_=tokio::time::sleep(std::time::Duration::from_secs(wait))=>{},changed=stop.changed()=>{if changed.is_err() || *stop.borrow(){return Ok(());}}}
    }
}

fn random_id() -> Result<String> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("OS entropy unavailable"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
impl OperatorGateway {
    pub fn new(config: OidcConfig, jwks: JwkSet, now: u64) -> Result<Self> {
        ensure!(
            config.issuer.starts_with("https://")
                && !config.audience.is_empty()
                && now < config.jwks_expires_unix
                && !config.required_amr.is_empty()
                && !config.allowed_acr.is_empty()
                && !jwks.keys.is_empty(),
            "configured OIDC issuer/audience/MFA and fresh JWKS required"
        );
        Ok(Self {
            config,
            jwks,
            challenges: BTreeMap::new(),
            sessions: BTreeMap::new(),
            control_roots: std::collections::BTreeSet::new(),
        })
    }
    /// Trusted Gateway routing hint. Current authority is always rechecked by the Kernel.
    pub(crate) fn cache_control_roots(&mut self, security: &SecurityState) {
        self.control_roots = security.policy.payload.root_humans.clone();
    }
    fn control_root_hint(&self, operator: &VerifiedOperator, now: u64) -> bool {
        self.control_roots.contains(&operator.id())
            && operator
                .require_step_up(now, self.config.max_step_up_age_seconds)
                .is_ok()
    }
    fn control_command_hint(
        &self,
        operator: &VerifiedOperator,
        command: &OperatorCommand,
        now: u64,
    ) -> bool {
        self.control_root_hint(operator, now)
            && matches!(
                command,
                OperatorCommand::EngageStop { .. }
                    | OperatorCommand::Module {
                        command: crate::runtime::ModuleCommand::Unload { .. }
                            | crate::runtime::ModuleCommand::RetryCleanup { .. }
                    }
            )
    }
    pub fn challenge(&mut self, now: u64) -> Result<Challenge> {
        self.challenges.retain(|_, expiry| now < *expiry);
        self.sessions.retain(|_, session| now < session.expires);
        ensure!(
            now < self.config.jwks_expires_unix && self.challenges.len() < 128,
            "OIDC trust stale or challenge capacity exhausted"
        );
        let nonce = random_id()?;
        let expires = now
            .checked_add(300)
            .context("authentication expiry overflow")?;
        self.challenges.insert(nonce.clone(), expires);
        Ok(Challenge {
            nonce,
            issuer: self.config.issuer.clone(),
            audience: self.config.audience.clone(),
            expires,
        })
    }
    pub fn login(&mut self, token: &str, nonce: &str, now: u64) -> Result<SessionHandle> {
        ensure!(
            self.challenges
                .get(nonce)
                .is_some_and(|expiry| now < *expiry),
            "unknown/expired/reused authentication challenge"
        );
        let operator = verify_operator_jwt(token, &self.config, &self.jwks, nonce, now)?;
        ensure!(
            self.sessions.len() < 1024,
            "operator session capacity exhausted"
        );
        let session_id = random_id()?;
        let expires = operator.expires_unix();
        let subject = operator.id();
        self.sessions.insert(
            session_id.clone(),
            Session {
                nonce: nonce.into(),
                token_hash: crate::audit::hash(token.as_bytes()),
                subject: subject.clone(),
                expires,
            },
        );
        self.challenges.remove(nonce);
        Ok(SessionHandle {
            session_id,
            expires,
            subject,
        })
    }
    pub fn authenticate(
        &self,
        session_id: &str,
        token: &str,
        now: u64,
    ) -> Result<VerifiedOperator> {
        let session = self
            .sessions
            .get(session_id)
            .context("unknown operator session")?;
        ensure!(
            now < session.expires && crate::audit::hash(token.as_bytes()) == session.token_hash,
            "session expired or token substitution"
        );
        let operator = verify_operator_jwt(token, &self.config, &self.jwks, &session.nonce, now)?;
        ensure!(operator.id() == session.subject, "operator subject changed");
        Ok(operator)
    }
    pub fn authorize_command(
        &self,
        operator: &VerifiedOperator,
        command: &OperatorCommand,
        security: &SecurityState,
        now: u64,
    ) -> Result<()> {
        let subject = AuthenticatedSubject::operator(operator);
        let (verb, resource, scope, cost, tier) = command.permission();
        ensure!(
            security.permitted(
                &subject,
                verb,
                resource,
                &scope,
                cost,
                tier,
                IsolationTier::P,
                now
            ),
            "operator RBAC/ABAC denied"
        );
        if tier == ConfirmationTier::C3 {
            operator.require_step_up(
                now,
                self.config
                    .max_step_up_age_seconds
                    .min(security.policy.payload.step_up_max_age),
            )?;
        }
        if let OperatorCommand::RegisterGoal { spec, posture, .. }
        | OperatorCommand::AmendGoal { spec, posture, .. } = command
        {
            ensure!(
                spec.goal_id == posture.id && spec.intent == posture.intent,
                "domain/security Goal contract mismatch"
            );
        }
        Ok(())
    }
}
#[derive(Clone)]
pub struct OperatorApi {
    pub gateway: Arc<Mutex<OperatorGateway>>,
    pub kernel: Arc<dyn OperatorKernel>,
    pub signer: Arc<CryptoSigner>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Login {
    nonce: String,
    token: String,
}
type ApiResult<T> = std::result::Result<Json<T>, (StatusCode, Json<Value>)>;
fn denied(status: StatusCode, message: &str) -> (StatusCode, Json<Value>) {
    (status, Json(serde_json::json!({"error":message})))
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
async fn challenge(State(api): State<OperatorApi>) -> ApiResult<Challenge> {
    api.gateway
        .lock()
        .await
        .challenge(now())
        .map(Json)
        .map_err(|_| {
            denied(
                StatusCode::SERVICE_UNAVAILABLE,
                "authentication configuration unavailable",
            )
        })
}
async fn session(
    State(api): State<OperatorApi>,
    Json(login): Json<Login>,
) -> ApiResult<SessionHandle> {
    let timestamp = now();
    let result = api
        .gateway
        .lock()
        .await
        .login(&login.token, &login.nonce, timestamp);
    let operator = if let Ok(session) = &result {
        Some(
            api.gateway
                .lock()
                .await
                .authenticate(&session.session_id, &login.token, timestamp)
                .map_err(|_| denied(StatusCode::UNAUTHORIZED, "operator authentication failed"))?,
        )
    } else {
        None
    };
    let id = format!("auth-{}", uuid::Uuid::new_v4());
    let accepted = result.is_ok();
    let reason_hash = crate::audit::hash(if accepted {
        b"OIDC verified session"
    } else {
        b"OIDC authentication refused"
    });
    let control_login = if let Some(human) = &operator {
        api.gateway.lock().await.control_root_hint(human, timestamp)
    } else {
        false
    };
    let audit = if control_login {
        api.kernel
            .audit_control_authentication(
                &id,
                operator.as_ref().unwrap(),
                reason_hash,
                &api.signer,
                timestamp,
            )
            .await
    } else {
        api.kernel
            .audit_authentication(
                &id,
                operator.as_ref(),
                accepted,
                reason_hash,
                &api.signer,
                timestamp,
            )
            .await
    };
    if audit.is_err() {
        if let Ok(session) = &result {
            api.gateway
                .lock()
                .await
                .sessions
                .remove(&session.session_id);
        }
        return Err(denied(
            StatusCode::SERVICE_UNAVAILABLE,
            "authentication audit unavailable",
        ));
    }
    result
        .map(Json)
        .map_err(|_| denied(StatusCode::UNAUTHORIZED, "operator authentication failed"))
}
async fn command(
    State(api): State<OperatorApi>,
    headers: HeaderMap,
    Json(request): Json<OperatorRequest>,
) -> ApiResult<Value> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| denied(StatusCode::UNAUTHORIZED, "operator authentication required"))?;
    let session = headers
        .get("x-swarm-session")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| denied(StatusCode::UNAUTHORIZED, "operator session required"))?;
    if request.request_id.is_empty() || request.request_id.len() > 200 {
        return Err(denied(
            StatusCode::BAD_REQUEST,
            "stable bounded request identity required",
        ));
    }
    let timestamp = now();
    let operator = api
        .gateway
        .lock()
        .await
        .authenticate(session, token, timestamp)
        .map_err(|_| denied(StatusCode::UNAUTHORIZED, "operator authentication failed"))?;
    let control_hint =
        api.gateway
            .lock()
            .await
            .control_command_hint(&operator, &request.operation, timestamp);
    let security = if control_hint {
        api.kernel
            .control_security_state(&operator, &request.operation, timestamp)
            .await
    } else {
        api.kernel.security_state().await
    }
    .map_err(|_| {
        denied(
            StatusCode::SERVICE_UNAVAILABLE,
            "authoritative state unavailable",
        )
    })?;
    let mut gateway = api.gateway.lock().await;
    gateway.cache_control_roots(&security);
    gateway
        .authorize_command(&operator, &request.operation, &security, timestamp)
        .map_err(|_| denied(StatusCode::FORBIDDEN, "operator action denied"))?;
    drop(gateway);
    api.kernel
        .apply_operator(operator, request, &api.signer, timestamp)
        .await
        .map(Json)
        .map_err(|_| denied(StatusCode::FORBIDDEN, "operator action denied"))
}
/// Caller serves this over a configured TLS listener; never mount it in an agent sandbox.
pub fn router(api: OperatorApi) -> Router {
    router_with_limit(api, 262144).expect("bounded default operator router")
}
pub fn router_with_limit(api: OperatorApi, max_request_bytes: usize) -> Result<Router> {
    ensure!(
        (1024..=32 * 1024 * 1024).contains(&max_request_bytes),
        "operator body ceiling invalid"
    );
    Ok(Router::new()
        .route("/operator/challenge", post(challenge))
        .route("/operator/session", post(session))
        .route("/operator/command", post(command))
        .layer(DefaultBodyLimit::max(max_request_bytes))
        .with_state(api))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_oidc_sessions_are_nonce_bound_expiring_and_single_use() {
        let timestamp = now();
        let issuer = crate::identity::TestOidcIssuer::new(timestamp);
        let mut gateway =
            OperatorGateway::new(issuer.config.clone(), issuer.jwks.clone(), timestamp).unwrap();
        let challenge = gateway.challenge(timestamp).unwrap();
        let token = issuer.issue("admin-a", &challenge.nonce, timestamp, true);
        let session = gateway.login(&token, &challenge.nonce, timestamp).unwrap();
        assert!(
            gateway
                .authenticate(&session.session_id, &token, timestamp)
                .is_ok()
        );
        assert!(gateway.login(&token, &challenge.nonce, timestamp).is_err());
        assert!(
            gateway
                .authenticate(&session.session_id, "wrong-token", timestamp)
                .is_err()
        );
        assert!(
            gateway
                .authenticate(&session.session_id, &token, timestamp + 901)
                .is_err()
        );
        let mut config = gateway.config.clone();
        config.jwks_expires_unix = timestamp;
        assert!(OperatorGateway::new(config, gateway.jwks.clone(), timestamp).is_err());
    }
}

#[cfg(test)]
mod refresh_tests {
    use super::*;
    use crate::identity::{IdentityAuthority, WorkloadId};
    use http_body_util::Full;
    use hyper::{Response, body::Bytes, service::service_fn};
    use hyper_util::rt::TokioIo;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use std::sync::atomic::{AtomicU8, Ordering};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    #[tokio::test]
    async fn live_https_discovery_refresh_rotates_keys_and_fails_closed() {
        let timestamp = now();
        let ca = IdentityAuthority::generate("oidc.test").unwrap();
        let leaf = ca
            .issue(
                WorkloadId {
                    trust_domain: "oidc.test".into(),
                    tenant: "t".into(),
                    goal: "g".into(),
                    node: "issuer".into(),
                    agent: "metadata".into(),
                    incarnation: "1".into(),
                },
                timestamp,
                300,
            )
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let origin = format!("https://node-issuer:{}", address.port());
        let mut issuer = crate::identity::TestOidcIssuer::new(timestamp);
        issuer.config.issuer = origin.clone();
        let keys = Arc::new(Mutex::new(serde_json::to_value(&issuer.jwks).unwrap()));
        let mode = Arc::new(AtomicU8::new(0));
        let acceptor = TlsAcceptor::from(Arc::new(
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    vec![CertificateDer::from(leaf.certificate_der.clone())],
                    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf.private_key_der.clone())),
                )
                .unwrap(),
        ));
        let serving = {
            let keys = keys.clone();
            let mode = mode.clone();
            let origin = origin.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    let acceptor = acceptor.clone();
                    let keys = keys.clone();
                    let mode = mode.clone();
                    let origin = origin.clone();
                    tokio::spawn(async move {
                        let stream = acceptor.accept(stream).await.unwrap();
                        let service = service_fn(
                            move |request: hyper::Request<hyper::body::Incoming>| {
                                let keys = keys.clone();
                                let mode = mode.clone();
                                let origin = origin.clone();
                                async move {
                                    let mode = mode.load(Ordering::SeqCst);
                                    let status = if mode == 1 {
                                        503
                                    } else if mode == 2 {
                                        302
                                    } else {
                                        200
                                    };
                                    let value = if request.uri().path()
                                        == "/.well-known/openid-configuration"
                                    {
                                        serde_json::json!({"issuer":if mode==3{"https://wrong.invalid"}else{origin.as_str()},"jwks_uri":format!("{origin}/{}",if mode==4{"changed-keys"}else{"keys"}),"id_token_signing_alg_values_supported":["EdDSA"]})
                                    } else {
                                        keys.lock().await.clone()
                                    };
                                    let bytes = if mode == 5 {
                                        vec![b' '; 262145]
                                    } else {
                                        serde_json::to_vec(&value).unwrap()
                                    };
                                    Ok::<_, std::convert::Infallible>(
                                        Response::builder()
                                            .status(status)
                                            .header("Content-Type", "application/json")
                                            .header("Cache-Control", "max-age=120")
                                            .header("Location", format!("{origin}/keys"))
                                            .body(Full::new(Bytes::from(bytes)))
                                            .unwrap(),
                                    )
                                }
                            },
                        );
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            })
        };
        let client = || {
            reqwest::Client::builder()
                .resolve("node-issuer", address)
                .add_root_certificate(
                    reqwest::Certificate::from_pem(leaf.ca_pem.as_bytes()).unwrap(),
                )
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .timeout(std::time::Duration::from_secs(2))
                .min_tls_version(reqwest::tls::Version::TLS_1_3)
                .build()
                .unwrap()
        };
        let mut refresher =
            JwksRefresher::new(&issuer.config, Some(leaf.ca_pem.as_bytes())).unwrap();
        refresher.client = client();
        let (fresh, expires) = refresher.refresh(timestamp).await.unwrap();
        assert!(expires > timestamp && expires <= timestamp + 120);
        let mut config = issuer.config.clone();
        config.jwks_expires_unix = expires;
        let mut gateway = OperatorGateway::new(config, fresh, timestamp).unwrap();
        let challenge = gateway.challenge(timestamp).unwrap();
        let token = issuer.issue("admin-a", &challenge.nonce, timestamp, true);
        let session = gateway.login(&token, &challenge.nonce, timestamp).unwrap();
        assert!(
            gateway
                .authenticate(&session.session_id, &token, timestamp)
                .is_ok()
        );
        let mut rotated = crate::identity::TestOidcIssuer::new(timestamp);
        rotated.config.issuer = origin;
        *keys.lock().await = serde_json::to_value(&rotated.jwks).unwrap();
        let (fresh, expires) = refresher.refresh(timestamp).await.unwrap();
        gateway.jwks = fresh;
        gateway.config.jwks_expires_unix = expires;
        assert!(
            gateway
                .authenticate(&session.session_id, &token, timestamp)
                .is_err()
        );
        let challenge = gateway.challenge(timestamp).unwrap();
        let token = rotated.issue("admin-a", &challenge.nonce, timestamp, true);
        let session = gateway.login(&token, &challenge.nonce, timestamp).unwrap();
        assert!(
            gateway
                .authenticate(&session.session_id, &token, timestamp)
                .is_ok()
        );
        for failure in [2, 3, 4, 5] {
            mode.store(failure, Ordering::SeqCst);
            assert!(refresher.refresh(timestamp).await.is_err());
        }
        mode.store(1, Ordering::SeqCst);
        let gateway = Arc::new(Mutex::new(gateway));
        let (stop, rx) = tokio::sync::watch::channel(false);
        let worker = tokio::spawn(refresh_jwks(gateway.clone(), refresher, rx));
        for _ in 0..200 {
            if gateway.lock().await.config.jwks_expires_unix == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(gateway.lock().await.challenge(timestamp).is_err());
        assert!(
            gateway
                .lock()
                .await
                .authenticate(&session.session_id, &token, timestamp)
                .is_err()
        );
        stop.send_replace(true);
        worker.await.unwrap().unwrap();
        serving.abort();
        let mut bad = issuer.config;
        bad.algorithms = ["HS256".into()].into();
        assert!(JwksRefresher::new(&bad, None).is_err());
    }
}
