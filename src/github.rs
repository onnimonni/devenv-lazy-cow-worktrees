//! GitHub side: auth token lookup, "cli" dev webhook lifecycle and the websocket
//! event stream. Protocol mirrors https://github.com/cli/gh-webhook.

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};
use tracing::{debug, info, warn};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoId {
    pub host: String,
    pub owner: String,
    pub name: String,
}

impl std::fmt::Display for RepoId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

/// Parse `git@host:o/r.git`, `https://host/o/r`, `ssh://git@host:22/o/r.git` etc.
pub fn parse_remote_url(url: &str) -> Option<RepoId> {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let (host, path) = if let Some((_, rest)) = url.split_once("://") {
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        (host.split(':').next()?, path)
    } else {
        let (authority, path) = url.split_once(':')?;
        (
            authority.rsplit_once('@').map_or(authority, |(_, h)| h),
            path,
        )
    };
    let (owner, name) = path.trim_start_matches('/').split_once('/')?;
    if host.is_empty() || owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some(RepoId {
        host: host.to_string(),
        owner: owner.to_string(),
        name: name.to_string(),
    })
}

/// Same lookup order as go-gh: env vars, the keyring entry `gh` writes, then its
/// hosts.yml (where gh keeps the token without a keyring, e.g. on Linux).
pub fn auth_token(host: &str) -> Result<String> {
    let env_vars: &[&str] = if host == "github.com" {
        &["GH_TOKEN", "GITHUB_TOKEN"]
    } else {
        &["GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"]
    };
    for var in env_vars {
        if let Ok(t) = std::env::var(var)
            && !t.trim().is_empty()
        {
            return Ok(t.trim().to_string());
        }
    }
    if let Ok(secret) =
        keyring::Entry::new(&format!("gh:{host}"), "").and_then(|e| e.get_password())
    {
        return decode_go_keyring(&secret);
    }
    // Without a keyring (typical on Linux) gh keeps the token in hosts.yml.
    let dir = std::env::var_os("GH_CONFIG_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_CONFIG_HOME").map(|d| std::path::PathBuf::from(d).join("gh"))
        })
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config/gh"))
        });
    if let Some(dir) = dir
        && let Ok(yml) = std::fs::read_to_string(dir.join("hosts.yml"))
        && let Some(t) = hosts_yml_token(&yml, host)
    {
        return Ok(t);
    }
    bail!("no token for {host}: set GH_TOKEN or run `gh auth login`")
}

/// `oauth_token` of `host` in gh's hosts.yml (top-level host keys, indented fields).
fn hosts_yml_token(yml: &str, host: &str) -> Option<String> {
    let mut in_host = false;
    for line in yml.lines() {
        if !line.starts_with(' ') && !line.starts_with('\t') {
            in_host = line.trim_end().trim_end_matches(':') == host;
            continue;
        }
        if in_host && let Some(v) = line.trim().strip_prefix("oauth_token:") {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// zalando/go-keyring stores secrets as `go-keyring-base64:<b64>` on macOS.
fn decode_go_keyring(secret: &str) -> Result<String> {
    match secret.strip_prefix("go-keyring-base64:") {
        Some(b64) => Ok(String::from_utf8(B64.decode(b64.trim())?)?),
        None => Ok(secret.to_string()),
    }
}

#[derive(Debug, Deserialize)]
pub struct Hook {
    pub id: u64,
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub ws_url: Option<String>,
}

pub struct Client {
    http: reqwest::Client,
    api: String,
    token: String,
    pub repo: RepoId,
}

impl Client {
    pub fn new(repo: RepoId, token: String) -> Result<Self> {
        let api = if repo.host == "github.com" {
            "https://api.github.com".to_string()
        } else {
            format!("https://{}/api/v3", repo.host)
        };
        let http = reqwest::Client::builder()
            .user_agent(concat!("localforest/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            http,
            api,
            token,
            repo,
        })
    }

    fn req(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    fn hooks_url(&self) -> String {
        format!(
            "{}/repos/{}/{}/hooks",
            self.api, self.repo.owner, self.repo.name
        )
    }

    /// Create an inactive `cli` hook. Replaces a stale one left by a crashed run.
    pub async fn create_hook(&self) -> Result<Hook> {
        let body = json!({
            "name": "cli",
            "events": ["push", "pull_request"],
            "active": false,
            "config": {"content_type": "json", "insecure_ssl": "0"},
        });
        for attempt in 0..2 {
            let resp = self
                .req(reqwest::Method::POST, &self.hooks_url())
                .json(&body)
                .send()
                .await?;
            let status = resp.status();
            if status.is_success() {
                return Ok(resp.json().await?);
            }
            let text = resp.text().await.unwrap_or_default();
            match status {
                StatusCode::UNPROCESSABLE_ENTITY
                    if attempt == 0 && text.contains("already exists") =>
                {
                    warn!(
                        "a cli webhook already exists on {} (stale run or `gh webhook forward`?), replacing it",
                        self.repo
                    );
                    self.delete_cli_hooks().await?;
                }
                StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => bail!(
                    "creating webhook on {} failed ({status}): need admin access to the repo and a token with repo/admin:repo_hook scope. {text}",
                    self.repo
                ),
                _ => bail!(
                    "creating webhook on {} failed ({status}): {text}",
                    self.repo
                ),
            }
        }
        unreachable!()
    }

    async fn delete_cli_hooks(&self) -> Result<()> {
        let hooks: Vec<Hook> = self
            .req(reqwest::Method::GET, &self.hooks_url())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        for h in hooks.into_iter().filter(|h| h.name == "cli") {
            self.delete_hook(&h).await?;
        }
        Ok(())
    }

    pub async fn activate_hook(&self, hook: &Hook) -> Result<()> {
        self.req(reqwest::Method::PATCH, &hook.url)
            .json(&json!({"active": true}))
            .send()
            .await?
            .error_for_status()
            .context("activating webhook")?;
        Ok(())
    }

    pub async fn delete_hook(&self, hook: &Hook) -> Result<()> {
        let resp = self.req(reqwest::Method::DELETE, &hook.url).send().await?;
        if !resp.status().is_success() && resp.status() != StatusCode::NOT_FOUND {
            bail!("deleting webhook {} failed: {}", hook.id, resp.status());
        }
        debug!("deleted webhook {}", hook.id);
        Ok(())
    }

    /// Connect to the hook's websocket, activate it, then call `on_event` for every
    /// push and merged pull request until the connection drops.
    pub async fn stream_events(&self, hook: &Hook, mut on_event: impl FnMut(Event)) -> Result<()> {
        let ws_url = hook
            .ws_url
            .as_deref()
            .ok_or_else(|| anyhow!("webhook response has no ws_url"))?;
        let mut req = ws_url.into_client_request()?;
        req.headers_mut()
            .insert("Authorization", HeaderValue::from_str(&self.token)?);
        let (mut ws, _) = tokio_tungstenite::connect_async(req)
            .await
            .context("connecting to webhook websocket")?;
        self.activate_hook(hook).await?;
        info!("listening for pushes on {}", self.repo);

        while let Some(msg) = ws.next().await {
            let data = match msg? {
                Message::Text(t) => t.as_bytes().to_vec(),
                Message::Binary(b) => b.to_vec(),
                Message::Close(frame) => {
                    info!("websocket closed by server: {frame:?}");
                    return Ok(());
                }
                _ => continue,
            };
            let ev: WsEvent = match serde_json::from_slice(&data) {
                Ok(ev) => ev,
                Err(e) => {
                    warn!("unparseable websocket message: {e}");
                    continue;
                }
            };
            match ev.event() {
                Ok(Some(e)) => on_event(e),
                Ok(None) => debug!("ignoring event {:?}", ev.header("X-GitHub-Event")),
                Err(e) => warn!("bad event payload: {e}"),
            }
            // GitHub records this as the delivery response.
            ws.send(Message::text(serde_json::to_string(&WsReply::ok())?))
                .await?;
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct WsEvent {
    #[serde(rename = "Header", default)]
    header: HashMap<String, Vec<String>>,
    /// Go `[]byte` => base64 string.
    #[serde(rename = "Body", default)]
    body: Option<String>,
}

impl WsEvent {
    fn header(&self, name: &str) -> Option<&str> {
        self.header
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .and_then(|(_, v)| v.first())
            .map(String::as_str)
    }

    /// Pushes and merged pull requests; None for everything else.
    fn event(&self) -> Result<Option<Event>> {
        let kind = self.header("X-GitHub-Event");
        if !matches!(kind, Some("push" | "pull_request")) {
            return Ok(None);
        }
        let body = B64.decode(self.body.as_deref().unwrap_or_default())?;
        let p: serde_json::Value = serde_json::from_slice(&body)?;
        if kind == Some("push") {
            return Ok(p["ref"].as_str().map(|r| Event::Push(r.to_string())));
        }
        let pr = &p["pull_request"];
        if p["action"] != "closed" || pr["merged"] != true {
            return Ok(None);
        }
        Ok(Some(Event::Merged {
            number: pr["number"].as_u64().unwrap_or_default(),
            branch: pr["head"]["ref"].as_str().unwrap_or_default().to_string(),
        }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// `refs/heads/x`
    Push(String),
    Merged {
        number: u64,
        branch: String,
    },
}

#[derive(Deserialize)]
struct PullHead {
    #[serde(rename = "ref")]
    branch: String,
    sha: String,
    repo: Option<PullRepo>,
}

#[derive(Deserialize)]
struct PullRepo {
    full_name: String,
}

#[derive(Deserialize)]
struct Pull {
    number: u64,
    merged_at: Option<String>,
    head: PullHead,
}

impl Client {
    /// Latest merged PR from this repository (not a fork) for `branch`: (number, head sha).
    pub async fn merged_pr(&self, branch: &str) -> Result<Option<(u64, String)>> {
        let url = format!(
            "{}/repos/{}/{}/pulls",
            self.api, self.repo.owner, self.repo.name
        );
        let head = format!("{}:{branch}", self.repo.owner);
        let pulls: Vec<Pull> = self
            .req(reqwest::Method::GET, &url)
            .query(&[
                ("state", "closed"),
                ("head", head.as_str()),
                ("per_page", "100"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let full = self.repo.to_string();
        Ok(pulls
            .into_iter()
            .filter(|p| {
                p.merged_at.is_some()
                    && p.head.branch == branch
                    && p.head
                        .repo
                        .as_ref()
                        .is_some_and(|r| r.full_name.eq_ignore_ascii_case(&full))
            })
            .max_by_key(|p| p.number)
            .map(|p| (p.number, p.head.sha)))
    }
}

#[derive(Serialize)]
struct WsReply {
    #[serde(rename = "Status")]
    status: u16,
    #[serde(rename = "Header")]
    header: HashMap<String, Vec<String>>,
    #[serde(rename = "Body")]
    body: String,
}

impl WsReply {
    fn ok() -> Self {
        Self {
            status: 200,
            header: HashMap::new(),
            body: B64.encode("OK"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(h: &str, o: &str, n: &str) -> Option<RepoId> {
        Some(RepoId {
            host: h.into(),
            owner: o.into(),
            name: n.into(),
        })
    }

    #[test]
    fn parses_remote_urls() {
        assert_eq!(
            parse_remote_url("git@github.com:onnimonni/localforest.git"),
            id("github.com", "onnimonni", "localforest")
        );
        assert_eq!(
            parse_remote_url("https://github.com/onnimonni/localforest"),
            id("github.com", "onnimonni", "localforest")
        );
        assert_eq!(
            parse_remote_url("https://x-access-token:abc@github.com/o/r.git/"),
            id("github.com", "o", "r")
        );
        assert_eq!(
            parse_remote_url("ssh://git@ghe.corp:2222/o/r.git"),
            id("ghe.corp", "o", "r")
        );
        assert_eq!(parse_remote_url("/tmp/some/bare.git"), None);
    }

    #[test]
    fn decodes_push_event() {
        let body = B64.encode(r#"{"ref":"refs/heads/main","after":"abc"}"#);
        let raw = format!(r#"{{"Header":{{"X-Github-Event":["push"]}},"Body":"{body}"}}"#);
        let ev: WsEvent = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            ev.event().unwrap(),
            Some(Event::Push("refs/heads/main".into()))
        );

        let ping = r#"{"Header":{"X-Github-Event":["ping"]},"Body":null}"#;
        let ev: WsEvent = serde_json::from_str(ping).unwrap();
        assert_eq!(ev.event().unwrap(), None);
    }

    #[test]
    fn decodes_merged_pull_request() {
        let body = B64.encode(
            r#"{"action":"closed","pull_request":{"number":7,"merged":true,"head":{"ref":"fix-it"}}}"#,
        );
        let raw = format!(r#"{{"Header":{{"X-Github-Event":["pull_request"]}},"Body":"{body}"}}"#);
        let ev: WsEvent = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            ev.event().unwrap(),
            Some(Event::Merged {
                number: 7,
                branch: "fix-it".into()
            })
        );
        let body = B64.encode(r#"{"action":"closed","pull_request":{"merged":false}}"#);
        let raw = format!(r#"{{"Header":{{"X-Github-Event":["pull_request"]}},"Body":"{body}"}}"#);
        let ev: WsEvent = serde_json::from_str(&raw).unwrap();
        assert_eq!(ev.event().unwrap(), None);
    }

    #[test]
    fn reads_gh_hosts_yml() {
        let yml = "github.com:\n    users:\n        onni:\n            oauth_token: gho_nested\n    oauth_token: gho_top\n    user: onni\nghe.corp:\n    oauth_token: \"gho_ghe\"\n";
        // The first oauth_token under the host (older files have it top-level).
        assert_eq!(
            hosts_yml_token(yml, "github.com").as_deref(),
            Some("gho_nested")
        );
        assert_eq!(hosts_yml_token(yml, "ghe.corp").as_deref(), Some("gho_ghe"));
        assert_eq!(hosts_yml_token(yml, "other.host"), None);
    }

    #[test]
    fn decodes_go_keyring_secret() {
        assert_eq!(
            decode_go_keyring("go-keyring-base64:Z2hvX3g=").unwrap(),
            "gho_x"
        );
        assert_eq!(decode_go_keyring("gho_plain").unwrap(), "gho_plain");
    }
}
