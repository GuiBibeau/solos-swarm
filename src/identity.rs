//! Offline-capable workload CA and strictly configured OIDC verification.
use ::time::OffsetDateTime;
use anyhow::{Context, Result, ensure};
use jsonwebtoken::{
    Algorithm, DecodingKey, Validation, decode, decode_header,
    jwk::{JwkSet, PublicKeyUse},
};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, SanType,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use x509_parser::{extensions::GeneralName, prelude::*};
use zeroize::Zeroize;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct WorkloadId {
    pub trust_domain: String,
    pub tenant: String,
    pub goal: String,
    pub node: String,
    pub agent: String,
    pub incarnation: String,
}
impl WorkloadId {
    pub fn validate(&self) -> Result<()> {
        for part in [
            &self.trust_domain,
            &self.tenant,
            &self.goal,
            &self.node,
            &self.agent,
            &self.incarnation,
        ] {
            ensure!(
                !part.is_empty()
                    && part.len() <= 128
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
                    && part != "."
                    && part != "..",
                "invalid SPIFFE identity segment"
            );
        }
        Ok(())
    }
    pub fn uri(&self) -> Result<String> {
        self.validate()?;
        Ok(format!(
            "spiffe://{}/tenant/{}/goal/{}/node/{}/agent/{}/incarnation/{}",
            self.trust_domain, self.tenant, self.goal, self.node, self.agent, self.incarnation
        ))
    }
    pub fn parse(uri: &str) -> Result<Self> {
        let rest = uri
            .strip_prefix("spiffe://")
            .context("not a SPIFFE identity")?;
        let p: Vec<_> = rest.split('/').collect();
        ensure!(
            p.len() == 11
                && p[1] == "tenant"
                && p[3] == "goal"
                && p[5] == "node"
                && p[7] == "agent"
                && p[9] == "incarnation",
            "invalid SPIFFE identity path"
        );
        let id = Self {
            trust_domain: p[0].into(),
            tenant: p[2].into(),
            goal: p[4].into(),
            node: p[6].into(),
            agent: p[8].into(),
            incarnation: p[10].into(),
        };
        id.validate()?;
        Ok(id)
    }
}

pub struct IdentityAuthority {
    trust_domain: String,
    issuer: Issuer<'static, KeyPair>,
    ca_der: Vec<u8>,
    ca_pem: String,
}
impl std::fmt::Debug for IdentityAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdentityAuthority")
            .field("trust_domain", &self.trust_domain)
            .field("key", &"[REDACTED]")
            .finish()
    }
}
pub struct IssuedIdentity {
    pub identity: WorkloadId,
    pub certificate_der: Vec<u8>,
    pub private_key_der: Vec<u8>,
    pub ca_der: Vec<u8>,
    pub certificate_pem: String,
    pub private_key_pem: String,
    pub ca_pem: String,
    pub server_name: String,
    pub expires_unix: u64,
}
/// An independently generated key remains with its workload; renewal returns public material only.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssuedCertificate {
    pub identity: WorkloadId,
    pub certificate_der: Vec<u8>,
    pub certificate_pem: String,
    pub ca_der: Vec<u8>,
    pub ca_pem: String,
    pub server_name: String,
    pub expires_unix: u64,
}
impl std::fmt::Debug for IssuedIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedIdentity")
            .field("identity", &self.identity)
            .field("expires_unix", &self.expires_unix)
            .field("key", &"[REDACTED]")
            .finish()
    }
}
impl Drop for IssuedIdentity {
    fn drop(&mut self) {
        self.private_key_der.zeroize();
        self.private_key_pem.zeroize();
    }
}
impl IdentityAuthority {
    pub fn generate(trust_domain: &str) -> Result<Self> {
        ensure!(
            !trust_domain.is_empty()
                && trust_domain
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-.".contains(&b)),
            "invalid trust domain"
        );
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(
            DnType::CommonName,
            format!("solos-swarm {trust_domain} workload CA"),
        );
        let ca = params.self_signed(&key)?;
        Ok(Self {
            trust_domain: trust_domain.into(),
            issuer: Issuer::new(params, key),
            ca_der: ca.der().to_vec(),
            ca_pem: ca.pem(),
        })
    }
    pub fn from_pem(trust_domain: &str, ca_pem: &str, private_key_pem: &str) -> Result<Self> {
        let (_, pem) = x509_parser::pem::parse_x509_pem(ca_pem.as_bytes())
            .map_err(|_| anyhow::anyhow!("invalid CA PEM"))?;
        let (_, ca) = parse_x509_certificate(&pem.contents)
            .map_err(|_| anyhow::anyhow!("invalid CA certificate"))?;
        ensure!(
            ca.is_ca() && ca.key_usage()?.is_some_and(|v| v.value.key_cert_sign()),
            "CA lacks certificate signing authority"
        );
        let key = KeyPair::from_pem(private_key_pem)?;
        ensure!(
            ca.public_key().subject_public_key.data.as_ref() == key.public_key_raw(),
            "CA key does not match certificate"
        );
        ca.verify_signature(None)?;
        Ok(Self {
            trust_domain: trust_domain.into(),
            issuer: Issuer::from_ca_cert_pem(ca_pem, key)?,
            ca_der: pem.contents.clone(),
            ca_pem: ca_pem.into(),
        })
    }
    pub fn ca_der(&self) -> &[u8] {
        &self.ca_der
    }
    pub fn ca_pem(&self) -> &str {
        &self.ca_pem
    }
    /// Caller stores this only in an operator-controlled secret store, never a state DTO.
    pub fn private_key_pem(&self) -> String {
        self.issuer.key().serialize_pem()
    }
    pub fn issue(
        &self,
        identity: WorkloadId,
        now_unix: u64,
        ttl_seconds: u64,
    ) -> Result<IssuedIdentity> {
        identity.validate()?;
        ensure!(
            identity.trust_domain == self.trust_domain,
            "identity trust domain mismatch"
        );
        ensure!(
            (1..=3600).contains(&ttl_seconds),
            "SVID lifetime must be <=1 hour"
        );
        let expires_unix = now_unix
            .checked_add(ttl_seconds)
            .context("identity expiry overflow")?;
        let server_name = format!("node-{}", identity.node);
        let key = KeyPair::generate_for(&rcgen::PKCS_ED25519)?;
        let mut params = CertificateParams::new(vec![server_name.clone()])?;
        params
            .subject_alt_names
            .push(SanType::URI(identity.uri()?.try_into()?));
        params.not_before = OffsetDateTime::from_unix_timestamp(now_unix.try_into()?)?;
        params.not_after = OffsetDateTime::from_unix_timestamp(expires_unix.try_into()?)?;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ClientAuth,
            ExtendedKeyUsagePurpose::ServerAuth,
        ];
        let cert = params.signed_by(&key, &self.issuer)?;
        verify_workload_certificate(cert.der(), &self.ca_der, &identity, now_unix)?;
        Ok(IssuedIdentity {
            identity,
            certificate_der: cert.der().to_vec(),
            certificate_pem: cert.pem(),
            private_key_der: key.serialize_der(),
            private_key_pem: key.serialize_pem(),
            ca_der: self.ca_der.clone(),
            ca_pem: self.ca_pem.clone(),
            server_name,
            expires_unix,
        })
    }
    pub fn issue_csr(
        &self,
        csr_pem: &str,
        identity: WorkloadId,
        pinned_public_key: &crate::audit::Hash,
        now_unix: u64,
        ttl_seconds: u64,
    ) -> Result<IssuedCertificate> {
        use rcgen::PublicKeyData;
        identity.validate()?;
        ensure!(
            identity.trust_domain == self.trust_domain
                && (1..=3600).contains(&ttl_seconds)
                && csr_pem.len() <= 32768
                && pinned_public_key != &[0; 32],
            "CSR issuance domain/key/lifetime bounds invalid"
        );
        let mut request = rcgen::CertificateSigningRequestParams::from_pem(csr_pem)?;
        ensure!(
            request.public_key.algorithm() == &rcgen::PKCS_ED25519
                && request.public_key.der_bytes() == pinned_public_key,
            "CSR key differs from independently admitted key"
        );
        let server_name = format!("node-{}", identity.node);
        let uri = identity.uri()?;
        let names = &request.params.subject_alt_names;
        ensure!(
            names.len() == 2
                && names
                    .iter()
                    .any(|name| matches!(name,SanType::URI(value) if value.as_str()==uri))
                && names.iter().any(
                    |name| matches!(name,SanType::DnsName(value) if value.as_str()==server_name)
                ),
            "CSR requests a different or ambiguous workload identity"
        );
        let expires_unix = now_unix
            .checked_add(ttl_seconds)
            .context("CSR identity expiry overflow")?;
        let mut params = CertificateParams::new(vec![server_name.clone()])?;
        params.subject_alt_names.push(SanType::URI(uri.try_into()?));
        params.not_before = OffsetDateTime::from_unix_timestamp(now_unix.try_into()?)?;
        params.not_after = OffsetDateTime::from_unix_timestamp(expires_unix.try_into()?)?;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ClientAuth,
            ExtendedKeyUsagePurpose::ServerAuth,
        ];
        request.params = params;
        let certificate = request.signed_by(&self.issuer)?;
        verify_workload_certificate(certificate.der(), &self.ca_der, &identity, now_unix)?;
        ensure!(
            workload_public_key(certificate.der())? == *pinned_public_key,
            "issued certificate changed the pinned key"
        );
        Ok(IssuedCertificate {
            identity,
            certificate_der: certificate.der().to_vec(),
            certificate_pem: certificate.pem(),
            ca_der: self.ca_der.clone(),
            ca_pem: self.ca_pem.clone(),
            server_name,
            expires_unix,
        })
    }
    pub fn rotate(
        &self,
        current: &IssuedIdentity,
        now_unix: u64,
        ttl_seconds: u64,
    ) -> Result<IssuedIdentity> {
        ensure!(
            now_unix < current.expires_unix,
            "expired identity requires re-attestation"
        );
        verify_workload_certificate(
            &current.certificate_der,
            &self.ca_der,
            &current.identity,
            now_unix,
        )?;
        self.issue(current.identity.clone(), now_unix, ttl_seconds)
    }
}
pub fn verify_workload_certificate(
    leaf_der: &[u8],
    ca_der: &[u8],
    expected: &WorkloadId,
    now_unix: u64,
) -> Result<()> {
    expected.validate()?;
    let (remaining, leaf) = parse_x509_certificate(leaf_der)
        .map_err(|_| anyhow::anyhow!("invalid workload certificate"))?;
    let (ca_remaining, ca) =
        parse_x509_certificate(ca_der).map_err(|_| anyhow::anyhow!("invalid trust anchor"))?;
    ensure!(
        remaining.is_empty() && ca_remaining.is_empty() && ca.is_ca() && !leaf.is_ca(),
        "invalid certificate authority/leaf"
    );
    let now = ASN1Time::from_timestamp(now_unix.try_into()?)?;
    ensure!(
        leaf.validity().is_valid_at(now) && ca.validity().is_valid_at(now),
        "certificate expired or not yet valid"
    );
    ensure!(
        leaf.validity().not_after.timestamp() - leaf.validity().not_before.timestamp() <= 3600,
        "SVID lifetime exceeds one hour"
    );
    ensure!(leaf.issuer() == ca.subject(), "workload issuer mismatch");
    leaf.verify_signature(Some(ca.public_key()))?;
    ensure!(
        ca.key_usage()?.is_some_and(|v| v.value.key_cert_sign())
            && leaf
                .key_usage()?
                .is_some_and(|v| v.value.digital_signature()),
        "invalid key usage"
    );
    let eku = leaf
        .extended_key_usage()?
        .context("missing mTLS key usage")?;
    ensure!(
        eku.value.client_auth && eku.value.server_auth,
        "workload certificate lacks mTLS usage"
    );
    let sans = leaf
        .subject_alternative_name()?
        .context("missing workload identity")?;
    let uris: Vec<_> = sans
        .value
        .general_names
        .iter()
        .filter_map(|n| {
            if let GeneralName::URI(uri) = n {
                Some(*uri)
            } else {
                None
            }
        })
        .collect();
    ensure!(uris == vec![expected.uri()?], "workload identity mismatch");
    let dns = format!("node-{}", expected.node);
    ensure!(
        sans.value
            .general_names
            .iter()
            .any(|n| matches!(n,GeneralName::DNSName(name) if *name==dns)),
        "node DNS identity mismatch"
    );
    Ok(())
}
pub fn workload_public_key(leaf_der: &[u8]) -> Result<[u8; 32]> {
    let (remaining, cert) = parse_x509_certificate(leaf_der)
        .map_err(|_| anyhow::anyhow!("invalid workload certificate"))?;
    ensure!(
        remaining.is_empty()
            && cert.public_key().algorithm.algorithm.to_id_string() == "1.3.101.112"
            && cert.public_key().algorithm.parameters.is_none(),
        "workload signing key must be Ed25519"
    );
    cert.public_key()
        .subject_public_key
        .data
        .as_ref()
        .try_into()
        .context("invalid Ed25519 key length")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcConfig {
    pub issuer: String,
    pub audience: String,
    pub algorithms: BTreeSet<String>,
    pub required_amr: BTreeSet<String>,
    pub allowed_acr: BTreeSet<String>,
    pub max_session_age_seconds: u64,
    pub max_step_up_age_seconds: u64,
    pub jwks_expires_unix: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OperatorClaims {
    pub sub: String,
    pub iss: String,
    pub aud: serde_json::Value,
    pub exp: u64,
    pub nbf: u64,
    pub iat: u64,
    pub auth_time: u64,
    pub nonce: String,
    pub principal_type: String,
    pub amr: BTreeSet<String>,
    pub acr: String,
    #[serde(default)]
    pub groups: BTreeSet<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct VerifiedOperator {
    claims: OperatorClaims,
}
impl VerifiedOperator {
    pub fn id(&self) -> String {
        format!("{}|{}", self.claims.iss, self.claims.sub)
    }
    pub fn groups(&self) -> &BTreeSet<String> {
        &self.claims.groups
    }
    pub fn expires_unix(&self) -> u64 {
        self.claims.exp
    }
    pub fn auth_time(&self) -> u64 {
        self.claims.auth_time
    }
    pub fn require_step_up(&self, now: u64, max_age: u64) -> Result<()> {
        ensure!(
            now < self.claims.exp
                && self.claims.auth_time <= now
                && now - self.claims.auth_time <= max_age,
            "fresh step-up authentication required"
        );
        Ok(())
    }
}
pub fn verify_operator_jwt(
    token: &str,
    config: &OidcConfig,
    jwks: &JwkSet,
    expected_nonce: &str,
    now: u64,
) -> Result<VerifiedOperator> {
    ensure!(
        token.len() <= 16_384 && !expected_nonce.is_empty() && now < config.jwks_expires_unix,
        "missing/stale OIDC trust configuration"
    );
    ensure!(
        !config.issuer.is_empty()
            && !config.audience.is_empty()
            && !config.required_amr.is_empty()
            && !config.allowed_acr.is_empty()
            && config.max_session_age_seconds > 0
            && config.max_step_up_age_seconds > 0,
        "OIDC MFA policy required"
    );
    let header = decode_header(token)?;
    ensure!(
        matches!(
            header.alg,
            Algorithm::RS256
                | Algorithm::RS384
                | Algorithm::RS512
                | Algorithm::PS256
                | Algorithm::PS384
                | Algorithm::PS512
                | Algorithm::ES256
                | Algorithm::ES384
                | Algorithm::EdDSA
        ),
        "symmetric/unsupported OIDC JWT algorithm"
    );
    ensure!(
        config.algorithms.contains(&format!("{:?}", header.alg)),
        "OIDC algorithm outside configured allowlist"
    );
    let kid = header.kid.context("OIDC key ID required")?;
    let matching: Vec<_> = jwks
        .keys
        .iter()
        .filter(|key| key.common.key_id.as_deref() == Some(&kid))
        .collect();
    ensure!(matching.len() == 1, "unknown or ambiguous OIDC key ID");
    let jwk = matching[0];
    ensure!(
        jwk.common
            .public_key_use
            .as_ref()
            .is_none_or(|v| *v == PublicKeyUse::Signature),
        "JWK is not a signing key"
    );
    ensure!(
        jwk.common
            .key_algorithm
            .as_ref()
            .is_some_and(|v| v.to_string() == format!("{:?}", header.alg)),
        "JWK/JWT algorithm mismatch"
    );
    let mut validation = Validation::new(header.alg);
    validation.leeway = 0;
    validation.validate_nbf = true;
    validation.set_audience(&[&config.audience]);
    validation.set_issuer(&[&config.issuer]);
    validation.set_required_spec_claims(&["exp", "nbf", "iss", "aud", "sub"]);
    let claims = decode::<OperatorClaims>(token, &DecodingKey::from_jwk(jwk)?, &validation)?.claims;
    ensure!(
        !claims.sub.is_empty()
            && claims.principal_type == "human"
            && claims.nonce == expected_nonce,
        "human OIDC session/nonce required"
    );
    ensure!(
        claims.nbf <= now
            && now < claims.exp
            && claims.iat <= now
            && now - claims.iat <= config.max_session_age_seconds
            && claims.exp - claims.iat <= config.max_session_age_seconds,
        "OIDC session expired/stale"
    );
    ensure!(
        claims.auth_time <= now
            && now - claims.auth_time <= config.max_session_age_seconds
            && config.required_amr.is_subset(&claims.amr)
            && config.allowed_acr.contains(&claims.acr),
        "phishing-resistant MFA policy not met"
    );
    Ok(VerifiedOperator { claims })
}

#[cfg(test)]
pub(crate) struct TestOidcIssuer {
    key: KeyPair,
    pub config: OidcConfig,
    pub jwks: JwkSet,
}
#[cfg(test)]
impl TestOidcIssuer {
    pub fn new(now: u64) -> Self {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        let key = KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
        let config = OidcConfig {
            issuer: "https://test-issuer.invalid".into(),
            audience: "swarm-test".into(),
            algorithms: ["EdDSA".into()].into(),
            required_amr: ["webauthn".into()].into(),
            allowed_acr: ["phishing-resistant".into()].into(),
            max_session_age_seconds: 900,
            max_step_up_age_seconds: 300,
            jwks_expires_unix: now + 900,
        };
        let jwks:JwkSet=serde_json::from_value(serde_json::json!({"keys":[{"kty":"OKP","crv":"Ed25519","alg":"EdDSA","use":"sig","kid":"test-key","x":URL_SAFE_NO_PAD.encode(key.public_key_raw())}]})).unwrap();
        Self { key, config, jwks }
    }
    pub(crate) fn fixture_private_key_pem(&self) -> String {
        self.key.serialize_pem()
    }
    pub fn issue(&self, subject: &str, nonce: &str, now: u64, mfa: bool) -> String {
        let claims = OperatorClaims {
            sub: subject.into(),
            iss: self.config.issuer.clone(),
            aud: serde_json::json!(self.config.audience),
            exp: now + 900,
            nbf: now.saturating_sub(1),
            iat: now,
            auth_time: now,
            nonce: nonce.into(),
            principal_type: "human".into(),
            amr: if mfa {
                ["webauthn".into()].into()
            } else {
                BTreeSet::new()
            },
            acr: "phishing-resistant".into(),
            groups: BTreeSet::new(),
        };
        let mut header = jsonwebtoken::Header::new(Algorithm::EdDSA);
        header.kid = Some("test-key".into());
        jsonwebtoken::encode(
            &header,
            &claims,
            &jsonwebtoken::EncodingKey::from_ed_der(&self.key.serialize_der()),
        )
        .unwrap()
    }
}
#[cfg(test)]
pub(crate) fn test_operator_token(
    subject: &str,
    now: u64,
    mfa: bool,
) -> (String, OidcConfig, JwkSet) {
    let issuer = TestOidcIssuer::new(now);
    let token = issuer.issue(subject, "test-nonce", now, mfa);
    (token, issuer.config, issuer.jwks)
}
#[cfg(test)]
pub(crate) fn test_operator(subject: &str, now: u64) -> VerifiedOperator {
    let (token, config, jwks) = test_operator_token(subject, now, true);
    verify_operator_jwt(&token, &config, &jwks, "test-nonce", now).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn id() -> WorkloadId {
        WorkloadId {
            trust_domain: "test.local".into(),
            tenant: "t".into(),
            goal: "g".into(),
            node: "n1".into(),
            agent: "a1".into(),
            incarnation: "i1".into(),
        }
    }
    #[test]
    fn workload_rotation_and_exact_identity_checks() {
        let ca = IdentityAuthority::generate("test.local").unwrap();
        let now = 1_800_000_000;
        let leaf = ca.issue(id(), now, 600).unwrap();
        verify_workload_certificate(&leaf.certificate_der, ca.ca_der(), &id(), now).unwrap();
        let mut wrong = id();
        wrong.agent = "a2".into();
        assert!(
            verify_workload_certificate(&leaf.certificate_der, ca.ca_der(), &wrong, now).is_err()
        );
        let other = IdentityAuthority::generate("test.local").unwrap();
        assert!(
            verify_workload_certificate(&leaf.certificate_der, other.ca_der(), &id(), now).is_err()
        );
        assert!(
            verify_workload_certificate(&leaf.certificate_der, ca.ca_der(), &id(), now + 601)
                .is_err()
        );
        let rotated = ca.rotate(&leaf, now + 300, 600).unwrap();
        assert_ne!(leaf.certificate_der, rotated.certificate_der);
        assert!(ca.issue(id(), now, 3601).is_err());
        let restored =
            IdentityAuthority::from_pem("test.local", ca.ca_pem(), &ca.private_key_pem()).unwrap();
        let restored_leaf = restored.issue(id(), now, 600).unwrap();
        verify_workload_certificate(&restored_leaf.certificate_der, ca.ca_der(), &id(), now)
            .unwrap();
        assert!(!format!("{leaf:?}").contains("PRIVATE KEY"));
    }
    #[test]
    fn jwt_rejects_algorithm_confusion_and_missing_mfa_config() {
        let config = OidcConfig {
            issuer: "https://issuer.invalid".into(),
            audience: "swarm".into(),
            algorithms: ["RS256".into()].into(),
            required_amr: ["webauthn".into()].into(),
            allowed_acr: ["phishing-resistant".into()].into(),
            max_session_age_seconds: 900,
            max_step_up_age_seconds: 300,
            jwks_expires_unix: u64::MAX,
        };
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(Algorithm::HS256),
            &serde_json::json!({"sub":"admin"}),
            &jsonwebtoken::EncodingKey::from_secret(b"not-an-oidc-key"),
        )
        .unwrap();
        assert!(
            verify_operator_jwt(&token, &config, &JwkSet { keys: vec![] }, "nonce", 1).is_err()
        );
        assert!(verify_operator_jwt("", &config, &JwkSet { keys: vec![] }, "", 1).is_err());
    }
    #[test]
    fn oidc_accepts_signed_human_mfa_and_denies_nonce_signature_and_staleness() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let (token, config, jwks) = test_operator_token("admin-a", now, true);
        let human = verify_operator_jwt(&token, &config, &jwks, "test-nonce", now).unwrap();
        assert!(human.id().ends_with("|admin-a"));
        assert!(verify_operator_jwt(&token, &config, &jwks, "wrong-nonce", now).is_err());
        let (mut forged, _, _) = test_operator_token("admin-b", now, true);
        forged.replace_range(forged.len() - 3.., "abc");
        assert!(verify_operator_jwt(&forged, &config, &jwks, "test-nonce", now).is_err());
        let (no_mfa, config_no_mfa, keys_no_mfa) = test_operator_token("admin-a", now, false);
        assert!(
            verify_operator_jwt(&no_mfa, &config_no_mfa, &keys_no_mfa, "test-nonce", now).is_err()
        );
        assert!(verify_operator_jwt(&token, &config, &jwks, "test-nonce", now + 901).is_err());
        human.require_step_up(now, 300).unwrap();
        assert!(human.require_step_up(now + 301, 300).is_err());
    }
}

#[cfg(test)]
mod csr_tests {
    use super::*;
    #[test]
    fn independent_key_renews_without_export_and_rejects_scope_or_key_changes() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let ca = IdentityAuthority::generate("csr.test").unwrap();
        let id = WorkloadId {
            trust_domain: "csr.test".into(),
            tenant: "t".into(),
            goal: "g".into(),
            node: "n1".into(),
            agent: "evaluator".into(),
            incarnation: "1".into(),
        };
        let leaf = ca.issue(id.clone(), now, 120).unwrap();
        // Only the protected workload reads this key to generate its proof of possession.
        let key = KeyPair::from_pem(&leaf.private_key_pem).unwrap();
        let mut params = CertificateParams::new(vec![leaf.server_name.clone()]).unwrap();
        params
            .subject_alt_names
            .push(SanType::URI(id.uri().unwrap().try_into().unwrap()));
        let csr = params.serialize_request(&key).unwrap().pem().unwrap();
        let pin = workload_public_key(&leaf.certificate_der).unwrap();
        let renewed = ca.issue_csr(&csr, id.clone(), &pin, now + 80, 120).unwrap();
        assert_eq!(renewed.expires_unix, now + 200);
        assert_eq!(workload_public_key(&renewed.certificate_der).unwrap(), pin);
        assert_ne!(renewed.certificate_der, leaf.certificate_der);
        let public = serde_json::to_value(&renewed).unwrap();
        assert!(public.get("private_key_pem").is_none());
        assert!(ca.issue_csr(&csr, id.clone(), &[9; 32], now, 120).is_err());
        let mut wrong = id.clone();
        wrong.goal = "other".into();
        assert!(ca.issue_csr(&csr, wrong, &pin, now, 120).is_err());
        assert!(ca.issue_csr(&csr, id.clone(), &pin, now, 3601).is_err());
        let mut altered = csr.into_bytes();
        let index = altered.len() / 2;
        altered[index] = if altered[index] == b'A' { b'B' } else { b'A' };
        assert!(
            ca.issue_csr(std::str::from_utf8(&altered).unwrap(), id, &pin, now, 120)
                .is_err()
        );
    }
}
