//! Publicly trusted certificates of projects with a domain (`lazyCowTree.tls.domain`).
//! The newest `https-certificate` Actions artifact of the project's GitHub
//! repository, a zip of combined PEM files (a chain, then its private key), one per
//! registered domain and per 100 names, made by
//! github.com/onnimonni/trusted-https-certificate-to-artifacts-action.
//! Only collaborators can read it, and it's only read while the repository is
//! private. The daemon keeps the certificates in memory; names none of them covers
//! get a leaf of the local CA.

use std::{collections::HashMap, io::Read, sync::Arc};

use anyhow::{Context, Result, anyhow, bail};
use parking_lot::Mutex;
use rustls::{
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    sign::CertifiedKey,
};

use crate::github;

/// The artifact: a zip of combined PEM files named after what they cover, e.g.
/// `_._.app.example.com.pem`.
pub const ARTIFACT: &str = "https-certificate";
/// The names the workflow requests, comma-separated (`cert show` prints them).
pub const DOMAINS_VARIABLE: &str = "HTTPS_CERTIFICATE_DOMAINS";
/// Bounds on what's read from the artifact: the action makes at most 20 PEM files
/// of a few kilobytes.
pub const MAX_ARTIFACT_BYTES: usize = 4 << 20;
const MAX_ENTRIES: usize = 20;
const MAX_ENTRY_BYTES: u64 = 256 << 10;

/// Names and validity of a certificate chain's leaf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leaf {
    pub names: Vec<String>,
    pub not_before: i64,
    pub not_after: i64,
}

impl Leaf {
    pub fn valid_at(&self, now: i64) -> bool {
        self.not_before <= now && now < self.not_after
    }

    /// It covers `name`: a host by name or wildcard, a wildcard only by itself.
    pub fn covers(&self, name: &str) -> bool {
        if name.starts_with("*.") {
            self.names.iter().any(|n| n == name)
        } else {
            self.rank(name).is_some()
        }
    }

    /// How well it covers `host`: 2 by name, 1 by a wildcard, else None.
    fn rank(&self, host: &str) -> Option<u8> {
        if self.names.iter().any(|n| n == host) {
            Some(2)
        } else if self.names.iter().any(|n| name_matches(n, host)) {
            Some(1)
        } else {
            None
        }
    }
}

/// `pattern` (a SAN, maybe `*.x`) matches `host`: a wildcard stands for one label.
pub fn name_matches(pattern: &str, host: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(rest) => host
            .split_once('.')
            .is_some_and(|(label, parent)| !label.is_empty() && parent == rest),
        None => pattern == host,
    }
}

pub fn pem_chain(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>> {
    let chain = CertificateDer::pem_slice_iter(pem).collect::<Result<Vec<_>, _>>()?;
    if chain.is_empty() {
        bail!("no certificate in it");
    }
    Ok(chain)
}

pub fn leaf(der: &[u8]) -> Result<Leaf> {
    use x509_parser::{extensions::GeneralName, prelude::*};
    let (_, cert) = X509Certificate::from_der(der).context("parsing the certificate")?;
    let names = cert
        .subject_alternative_name()?
        .map(|san| {
            san.value
                .general_names
                .iter()
                .filter_map(|n| match n {
                    GeneralName::DNSName(d) => Some(d.to_ascii_lowercase()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Leaf {
        names,
        not_before: cert.validity().not_before.timestamp(),
        not_after: cert.validity().not_after.timestamp(),
    })
}

/// The GitHub repository with the certificates, with the project's token:
/// `lazyCowTree.tls.githubRepository`, else the project's remote's.
pub fn repo_client(project: &crate::config::Project) -> Result<github::Client> {
    if let Some(r) = project.settings.tls_github_repository.as_deref() {
        let repo =
            github::parse_repo(r).ok_or_else(|| anyhow!("{r} is not a GitHub repository"))?;
        let token = github::auth_token(&repo.host, &project.env)?;
        return github::Client::new(repo, token);
    }
    let remote = &project.settings.remote;
    let repo = git2::Repository::open(&project.root)?
        .find_remote(remote)?
        .url()
        .ok()
        .and_then(github::parse_remote_url)
        .ok_or_else(|| {
            anyhow!(
                "the certificate lives in a GitHub repository's artifacts: no GitHub remote {remote}"
            )
        })?;
    let token = github::auth_token(&repo.host, &project.env)?;
    github::Client::new(repo, token)
}

/// The artifact (zip bytes), only once the repository is known to be private.
/// None: not issued yet.
pub async fn fetch(gh: &github::Client) -> Result<Option<Vec<u8>>> {
    if !gh.is_private().await? {
        bail!("{} is public: refusing to read its private keys", gh.repo);
    }
    gh.artifact(ARTIFACT, MAX_ARTIFACT_BYTES).await
}

/// A certificate chain with its key.
#[derive(Clone)]
pub struct Cert {
    /// Its file in the artifact, e.g. `_._.app.example.com.pem`.
    pub file: String,
    pub leaf: Leaf,
    pub key: Arc<CertifiedKey>,
}

/// The certificate chain and key of a combined PEM file, checked to belong together.
pub fn certified_key(pem: &str) -> Result<Cert> {
    let chain = pem_chain(pem.as_bytes())?;
    let leaf = leaf(&chain[0])?;
    let der = PrivateKeyDer::from_pem_slice(pem.as_bytes()).context("no private key in it")?;
    let signing =
        rustls::crypto::aws_lc_rs::sign::any_supported_type(&der).context("unsupported key")?;
    let key = CertifiedKey::new(chain, signing);
    key.keys_match()
        .map_err(|e| anyhow!("its certificate is not for its key ({e})"))?;
    Ok(Cert {
        file: String::new(),
        leaf,
        key: Arc::new(key),
    })
}

/// Every `.pem` of the artifact's zip, read in memory within the bounds above.
pub fn certificates(zip: &[u8]) -> Result<Vec<Cert>> {
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(zip)).context("reading the artifact's zip")?;
    if archive.len() > MAX_ENTRIES {
        bail!(
            "{} files in the artifact, at most {MAX_ENTRIES}",
            archive.len()
        );
    }
    let mut certs = Vec::new();
    for i in 0..archive.len() {
        let entry = archive.by_index(i)?;
        let name = entry.name().to_string();
        if entry.is_dir() || !name.to_ascii_lowercase().ends_with(".pem") {
            continue;
        }
        if entry.size() > MAX_ENTRY_BYTES {
            bail!("{name}: over {MAX_ENTRY_BYTES} bytes");
        }
        let mut pem = String::new();
        entry.take(MAX_ENTRY_BYTES + 1).read_to_string(&mut pem)?;
        if pem.len() as u64 > MAX_ENTRY_BYTES {
            bail!("{name}: over {MAX_ENTRY_BYTES} bytes");
        }
        let mut cert = certified_key(&pem).with_context(|| name.clone())?;
        cert.file = name;
        certs.push(cert);
    }
    if certs.is_empty() {
        bail!("no .pem file in the artifact");
    }
    Ok(certs)
}

/// `names` in bash brace form, the way the action reads them: sibling labels whose
/// names below are the same merge, e.g. `1.4.x.com` … `3.6.x.com` into
/// `{1,2,3}.{4,5,6}.x.com`.
pub fn brace_names(names: &[String]) -> Vec<String> {
    #[derive(Default)]
    struct Node {
        name: bool,
        children: std::collections::BTreeMap<String, Node>,
    }
    fn braces(node: &Node) -> Vec<String> {
        let mut groups: std::collections::BTreeMap<Vec<String>, Vec<&str>> = Default::default();
        for (label, child) in &node.children {
            groups.entry(braces(child)).or_default().push(label);
        }
        let mut out: Vec<String> = node.name.then(String::new).into_iter().collect();
        for (below, labels) in groups {
            let label = match labels.as_slice() {
                [one] => one.to_string(),
                many => format!("{{{}}}", many.join(",")),
            };
            out.extend(below.iter().map(|prefix| {
                if prefix.is_empty() {
                    label.clone()
                } else {
                    format!("{prefix}.{label}")
                }
            }));
        }
        out.sort();
        out
    }
    let mut root = Node::default();
    for name in names {
        let node = name.rsplit('.').fold(&mut root, |node, label| {
            node.children.entry(label.to_string()).or_default()
        });
        node.name = true;
    }
    braces(&root)
}

pub fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// domain -> its project's certificates, once read.
#[derive(Default)]
pub struct Trusted {
    domains: Mutex<HashMap<String, Vec<Cert>>>,
}

impl std::fmt::Debug for Trusted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Trusted")
    }
}

impl Trusted {
    /// Its names are served (by the local CA until certificates are set).
    pub fn register(&self, domain: &str) {
        self.domains.lock().entry(domain.to_string()).or_default();
    }

    pub fn set(&self, domain: &str, certs: Vec<Cert>) {
        self.domains.lock().insert(domain.to_string(), certs);
    }

    /// Stop serving its certificates (the local CA takes over).
    pub fn clear(&self, domain: &str) {
        self.set(domain, Vec::new());
    }

    /// `host` is under a registered domain.
    pub fn owns(&self, host: &str) -> bool {
        self.domains.lock().keys().any(|d| under(host, d))
    }

    /// The certificate for `host` at `now`: of the most specific registered domain
    /// above it, a valid one naming it, else one covering it by a wildcard; the
    /// newest of equals.
    pub fn resolve_at(&self, host: &str, now: i64) -> Option<Arc<CertifiedKey>> {
        let domains = self.domains.lock();
        let (_, certs) = domains
            .iter()
            .filter(|(d, _)| under(host, d))
            .max_by_key(|(d, _)| d.len())?;
        certs
            .iter()
            .filter(|c| c.leaf.valid_at(now))
            .filter_map(|c| Some(((c.leaf.rank(host)?, c.leaf.not_before), c)))
            .max_by_key(|(order, _)| *order)
            .map(|(_, c)| c.key.clone())
    }

    pub fn resolve(&self, host: &str) -> Option<Arc<CertifiedKey>> {
        self.resolve_at(host, now())
    }
}

fn under(host: &str, domain: &str) -> bool {
    host.strip_suffix(domain)
        .is_some_and(|rest| rest.is_empty() || rest.ends_with('.'))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn bundle(names: &[&str], days: std::ops::Range<i64>) -> String {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params =
            rcgen::CertificateParams::new(names.iter().map(|n| n.to_string()).collect::<Vec<_>>())
                .unwrap();
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now + time::Duration::days(days.start);
        params.not_after = now + time::Duration::days(days.end);
        let cert = params.self_signed(&key).unwrap();
        format!("{}{}", cert.pem(), key.serialize_pem())
    }

    fn zip(files: &[(&str, &str)]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, body) in files {
            w.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(body.as_bytes()).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    /// The action's expansion, for checking round trips.
    fn expand(pattern: &str) -> Vec<String> {
        let Some(open) = pattern.rfind('{') else {
            return vec![pattern.to_string()];
        };
        let close = open + pattern[open..].find('}').unwrap();
        pattern[open + 1..close]
            .split(',')
            .flat_map(|alt| {
                expand(&format!(
                    "{}{alt}{}",
                    &pattern[..open],
                    &pattern[close + 1..]
                ))
            })
            .collect()
    }

    #[test]
    fn brace_names_round_trip() {
        let names: Vec<String> = ["1", "2", "3"]
            .iter()
            .flat_map(|a| ["4", "5", "6"].map(|b| format!("{a}.{b}.dev.example.com")))
            .chain([
                "example.com".into(),
                "*.example.com".into(),
                "*.web.app.other.org".into(),
            ])
            .collect();
        let braced = brace_names(&names);
        assert_eq!(
            braced,
            [
                "*.example.com",
                "*.web.app.other.org",
                "example.com",
                "{1,2,3}.{4,5,6}.dev.example.com"
            ]
        );
        let mut back: Vec<String> = braced.iter().flat_map(|p| expand(p)).collect();
        let mut want = names.clone();
        back.sort();
        want.sort();
        assert_eq!(back, want);
    }

    #[test]
    fn wildcards_stand_for_one_label() {
        assert!(name_matches("web.app.dev.test", "web.app.dev.test"));
        assert!(name_matches("*.web.app.dev.test", "wt.web.app.dev.test"));
        assert!(!name_matches("*.web.app.dev.test", "web.app.dev.test"));
        assert!(!name_matches("*.web.app.dev.test", "a.wt.web.app.dev.test"));
        assert!(under("wt.web.app.dev.test", "dev.test"));
        assert!(under("dev.test", "dev.test"));
        assert!(!under("xdev.test", "dev.test"));
    }

    #[test]
    fn a_bundle_needs_its_own_key() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let pem = bundle(&["web.app.dev.test", "*.web.app.dev.test"], -1..60);
        let cert_only = &pem[..pem.find("-----BEGIN PRIVATE KEY").unwrap()];
        let other = rcgen::KeyPair::generate().unwrap();
        assert!(certified_key(&format!("{cert_only}{}", other.serialize_pem())).is_err());
        assert!(certified_key(cert_only).is_err());
        let c = certified_key(&pem).unwrap();
        assert_eq!(c.leaf.names, ["web.app.dev.test", "*.web.app.dev.test"]);
    }

    #[test]
    fn reads_every_pem_of_the_zip_and_picks_by_name() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let wide = bundle(&["*.web.app.dev.test", "web.app.dev.test"], -1..60);
        let exact = bundle(&["a.web.app.dev.test"], -1..60);
        let expired = bundle(&["b.web.app.dev.test"], -100..-10);
        let certs = certificates(&zip(&[
            ("https-certificate-1.pem", &wide),
            ("https-certificate-2.pem", &exact),
            ("https-certificate-3.PEM", &expired),
            ("notes.txt", "not a certificate"),
        ]))
        .unwrap();
        assert_eq!(certs.len(), 3);
        let key_of = |names: &[&str]| {
            certs
                .iter()
                .find(|c| c.leaf.names == names)
                .unwrap()
                .key
                .clone()
        };

        let t = Trusted::default();
        t.register("dev.test");
        assert!(t.owns("wt.api.app.dev.test"));
        // Nothing read yet: the local CA's turn.
        assert!(t.resolve("wt.web.app.dev.test").is_none());
        t.set("dev.test", certs.clone());
        // By name beats by wildcard.
        let got = t.resolve("a.web.app.dev.test").unwrap();
        assert!(Arc::ptr_eq(&got, &key_of(&["a.web.app.dev.test"])));
        let got = t.resolve("wt.web.app.dev.test").unwrap();
        assert!(Arc::ptr_eq(
            &got,
            &key_of(&["*.web.app.dev.test", "web.app.dev.test"])
        ));
        // The expired one naming it loses to the valid wildcard.
        let got = t.resolve("b.web.app.dev.test").unwrap();
        assert!(Arc::ptr_eq(
            &got,
            &key_of(&["*.web.app.dev.test", "web.app.dev.test"])
        ));
        assert!(t.resolve("wt.api.app.dev.test").is_none());
        // Covering: a host by name or wildcard, a wildcard only by itself.
        let wide = &key_of(&["*.web.app.dev.test", "web.app.dev.test"]);
        let leaf = &certs
            .iter()
            .find(|c| Arc::ptr_eq(&c.key, wide))
            .unwrap()
            .leaf;
        assert!(leaf.covers("web.app.dev.test") && leaf.covers("wt.web.app.dev.test"));
        assert!(leaf.covers("*.web.app.dev.test") && !leaf.covers("*.wt.web.app.dev.test"));
        // Once they expire, nothing: the local CA's turn again.
        assert!(
            t.resolve_at("a.web.app.dev.test", now() + 90 * 86400)
                .is_none()
        );
        t.clear("dev.test");
        assert!(t.resolve("a.web.app.dev.test").is_none());
    }

    #[test]
    fn the_most_specific_domain_wins() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let outer = certified_key(&bundle(&["*.app.dev.test"], -1..60)).unwrap();
        let inner = certified_key(&bundle(&["*.app.dev.test"], -1..60)).unwrap();
        let t = Trusted::default();
        t.set("dev.test", vec![outer]);
        t.set("app.dev.test", vec![inner.clone()]);
        let got = t.resolve("x.app.dev.test").unwrap();
        assert!(Arc::ptr_eq(&got, &inner.key));
    }

    #[test]
    fn refuses_bad_artifacts() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        assert!(certificates(b"not a zip").is_err());
        assert!(certificates(&zip(&[("readme.txt", "x")])).is_err());
        let many: Vec<(String, String)> = (0..=MAX_ENTRIES)
            .map(|i| (format!("{i}.txt"), String::new()))
            .collect();
        let many: Vec<(&str, &str)> = many.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        assert!(certificates(&zip(&many)).is_err());
        let big = "x".repeat(MAX_ENTRY_BYTES as usize + 1);
        assert!(certificates(&zip(&[("big.pem", &big)])).is_err());
    }
}
