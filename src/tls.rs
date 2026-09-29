//! Local certificate authority for https://*.localhost. The CA is created once in
//! the state directory; leaf certificates are issued in memory per SNI name.

use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose,
};
use rustls::{
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};
use time::{Duration, OffsetDateTime};

pub struct Ca {
    issuer: Issuer<'static, KeyPair>,
    cert_der: CertificateDer<'static>,
    leaves: Mutex<HashMap<String, Arc<CertifiedKey>>>,
}

/// The CA's common name. Leaves name it as their issuer, so a CA created under another
/// name (the project's former `localforest`) is replaced rather than reused.
const CA_NAME: &str = "lazy-cow-tree local development CA";

fn ca_params() -> CertificateParams {
    let mut p = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, CA_NAME);
    dn.push(DnType::OrganizationName, "lazy-cow-tree");
    p.distinguished_name = dn;
    p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    p.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    p
}

impl Ca {
    /// Load the CA from `dir` (ca.pem, ca-key.pem), creating it the first time.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        let cert_path = dir.join("ca.pem");
        let key_path = dir.join("ca-key.pem");
        let existing = (cert_path.exists() && key_path.exists())
            .then(|| std::fs::read_to_string(&cert_path))
            .transpose()?
            .filter(|pem| {
                // DER keeps the common name's bytes verbatim.
                let current = rustls_pemfile_cert(pem).is_ok_and(|der| {
                    der.windows(CA_NAME.len()).any(|w| w == CA_NAME.as_bytes())
                });
                if !current {
                    tracing::warn!(
                        "replacing the local CA in {} (created under another name); run `lazy-cow-tree trust` again",
                        dir.display()
                    );
                }
                current
            });
        let (cert_pem, key) = if let Some(pem) = existing {
            let key = KeyPair::from_pem(&std::fs::read_to_string(&key_path)?)?;
            (pem, key)
        } else {
            std::fs::create_dir_all(dir)?;
            let key = KeyPair::generate()?;
            let mut params = ca_params();
            let now = OffsetDateTime::now_utc();
            params.not_before = now - Duration::days(1);
            params.not_after = now + Duration::days(3650);
            let cert = params.self_signed(&key)?;
            write_private(&key_path, key.serialize_pem().as_bytes())?;
            std::fs::write(&cert_path, cert.pem())?;
            tracing::info!(
                "created local CA {}; run `lazy-cow-tree trust` to trust it",
                cert_path.display()
            );
            (cert.pem(), key)
        };
        let cert_der = rustls_pemfile_cert(&cert_pem)?;
        Ok(Self {
            issuer: Issuer::new(ca_params(), key),
            cert_der,
            leaves: Mutex::new(HashMap::new()),
        })
    }

    /// For the keychain (`trust`), macOS only.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn cert_der(&self) -> &CertificateDer<'static> {
        &self.cert_der
    }

    fn leaf(&self, host: &str) -> Result<Arc<CertifiedKey>> {
        if let Some(k) = self.leaves.lock().unwrap().get(host) {
            return Ok(k.clone());
        }
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::new(vec![host.to_string()])?;
        params.distinguished_name.push(DnType::CommonName, host);
        let now = OffsetDateTime::now_utc();
        // Apple rejects server certificates valid for more than 825 days.
        params.not_before = now - Duration::days(1);
        params.not_after = now + Duration::days(365);
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        params.use_authority_key_identifier_extension = true;
        let cert = params.signed_by(&key, &self.issuer)?;
        let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        let signing = rustls::crypto::aws_lc_rs::sign::any_supported_type(&key_der)
            .context("unsupported key")?;
        let ck = Arc::new(CertifiedKey::new(
            vec![cert.der().clone(), self.cert_der.clone()],
            signing,
        ));
        self.leaves
            .lock()
            .unwrap()
            .insert(host.to_string(), ck.clone());
        Ok(ck)
    }
}

fn rustls_pemfile_cert(pem: &str) -> Result<CertificateDer<'static>> {
    use base64::Engine;
    let b64: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect::<Vec<_>>()
        .concat();
    let der = base64::engine::general_purpose::STANDARD.decode(b64.trim())?;
    Ok(CertificateDer::from(der))
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(data)?;
    Ok(())
}

impl std::fmt::Debug for Ca {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Ca")
    }
}

impl ResolvesServerCert for Ca {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let host = hello
            .server_name()
            .unwrap_or("localhost")
            .to_ascii_lowercase();
        if host != "localhost" && !host.ends_with(".localhost") {
            return None;
        }
        self.leaf(&host)
            .map_err(|e| tracing::warn!("certificate for {host}: {e:#}"))
            .ok()
    }
}

pub fn server_config(certificates: Arc<dyn ResolvesServerCert>) -> rustls::ServerConfig {
    let mut cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(certificates);
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    cfg
}

/// Add the CA to the login keychain and trust it for TLS (macOS asks for the
/// password). Uses the Security framework directly, no `security` subprocess.
#[cfg(target_os = "macos")]
pub fn trust(ca: &Ca) -> Result<()> {
    use security_framework::{
        certificate::SecCertificate,
        item::{ItemAddOptions, ItemAddValue},
        trust_settings::{Domain, TrustSettings},
    };
    let cert = SecCertificate::from_der(ca.cert_der())?;
    // errSecDuplicateItem (-25299): already in the keychain.
    if let Err(e) = ItemAddOptions::new(ItemAddValue::Ref(
        security_framework::item::AddRef::Certificate(cert.clone()),
    ))
    .add()
        && e.code() != -25299
    {
        return Err(e).context("adding the CA to the login keychain");
    }
    TrustSettings::new(Domain::User)
        .set_trust_settings_always(&cert)
        .context("trusting the CA")?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn trust(_ca: &Ca) -> Result<()> {
    anyhow::bail!(
        "automatic trust is macOS only; add {} to your system and browser trust stores",
        crate::config::ca_cert_path().display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_its_own_ca_and_replaces_one_made_under_another_name() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let d = tempfile::tempdir().unwrap();
        let first = Ca::load_or_create(d.path()).unwrap();
        let again = Ca::load_or_create(d.path()).unwrap();
        assert_eq!(first.cert_der(), again.cert_der());

        // A CA like the former name's: same files, another common name.
        let key = KeyPair::generate().unwrap();
        let mut params = ca_params();
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, "localforest local development CA");
        let old = params.self_signed(&key).unwrap();
        std::fs::write(d.path().join("ca.pem"), old.pem()).unwrap();
        std::fs::write(d.path().join("ca-key.pem"), key.serialize_pem()).unwrap();
        let replaced = Ca::load_or_create(d.path()).unwrap();
        assert_ne!(replaced.cert_der().as_ref(), old.der().as_ref());
        let pem = std::fs::read_to_string(d.path().join("ca.pem")).unwrap();
        let der = rustls_pemfile_cert(&pem).unwrap();
        assert!(der.windows(CA_NAME.len()).any(|w| w == CA_NAME.as_bytes()));
    }
}
