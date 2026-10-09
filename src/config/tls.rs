//! Settings of a project's own domain (`lazyCowTree.tls`): its names and the ones
//! its certificates should cover.

use super::{Project, valid_label};

/// `ProjectSettings::tls_services` by default: common names of web services.
pub const TLS_SERVICES: &str = "app,web,www,api,backend,frontend,admin,dashboard,auth,docs,storybook,simulator,mobile,cms,mail,assets,vite,ws";

pub(crate) fn default_tls_services() -> Vec<String> {
    TLS_SERVICES.split(',').map(str::to_string).collect()
}

/// The certificate action's limit: 5 certificates of 100 names.
pub(crate) const MAX_SANS: usize = 500;

pub(crate) fn parse_domain(s: &str) -> std::result::Result<String, String> {
    let d = s.trim_end_matches('.').to_ascii_lowercase();
    if d.contains('.') && d.split('.').all(valid_label) {
        Ok(d)
    } else {
        Err(format!("{s} is not a domain name"))
    }
}

pub(crate) fn parse_repository(s: &str) -> std::result::Result<String, String> {
    crate::github::parse_repo(s)
        .map(|r| format!("{}/{}/{}", r.host, r.owner, r.name))
        .ok_or_else(|| format!("{s} is not a GitHub repository (owner/repo)"))
}

impl Project {
    /// Services' own hostnames are `.localhost` ones (the local CA's) or under the
    /// project's tls domain (its certificate's).
    pub fn check_hostnames(&self) -> std::result::Result<(), String> {
        let domain = self.settings.tls_domain.as_deref();
        for (name, s) in &self.settings.services.0 {
            let Some(h) = &s.hostname else { continue };
            let under = |d: &str| h.strip_suffix(d).is_some_and(|r| r.ends_with('.'));
            if !under("localhost") && !domain.is_some_and(under) {
                return Err(match domain {
                    Some(d) => {
                        format!("service {name}: hostname {h} must end in .localhost or .{d}")
                    }
                    None => format!(
                        "service {name}: hostname {h} must end in .localhost (or under lazyCowTree.tls.domain)"
                    ),
                });
            }
        }
        Ok(())
    }

    /// Names its domain's certificates need, each with a wildcard for its worktrees':
    /// every http host of the primary checkout under the domain, then the project's
    /// own and `<service>.<project>.<domain>` of `tls_services` (as many as fit).
    /// Names a wildcard of the list covers are left out: `*.<project>.<domain>`
    /// covers `<service>.<project>.<domain>`.
    pub fn tls_names(&self) -> Vec<String> {
        let Some(domain) = &self.settings.tls_domain else {
            return Vec::new();
        };
        let suffix = format!(".{domain}");
        let hosts = self
            .checkout_on(None, self.root.clone(), self.settings.port)
            .routes()
            .into_iter()
            .map(|(_, host, _)| host)
            .filter(|h| h.ends_with(&suffix));
        let common = std::iter::once(format!("{}.{domain}", self.name)).chain(
            self.settings
                .tls_services
                .iter()
                .filter(|s| valid_label(s))
                .map(|s| format!("{s}.{}.{domain}", self.name)),
        );
        let mut names: Vec<String> = Vec::new();
        for h in hosts.chain(common) {
            if !names.contains(&h) {
                names.push(format!("*.{h}"));
                names.push(h);
            }
        }
        let covered = |n: &String| {
            !n.starts_with("*.")
                && names
                    .iter()
                    .any(|w| w.starts_with("*.") && crate::tls::trusted::name_matches(w, n))
        };
        let mut names: Vec<String> = names.iter().filter(|n| !covered(n)).cloned().collect();
        names.truncate(MAX_SANS);
        names.sort();
        names
    }
}
