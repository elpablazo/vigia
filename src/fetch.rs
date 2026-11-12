//! Fetching pages: directly from this machine, or through an agent running on another network.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, ToSocketAddrs};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Notify, oneshot};

pub const DEFAULT_UA: &str = concat!("Vigia/", env!("CARGO_PKG_VERSION"), " (monitor personal de cambios)");
const MAX_BODY: usize = 4 * 1024 * 1024;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Page {
    pub status: u16,
    /// The site answered with an anti-bot challenge instead of the page.
    pub challenge: bool,
    pub body: String,
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 8 {
                return attempt.error("demasiadas redirecciones");
            }
            match public_host(attempt.url()) {
                Ok(()) => attempt.follow(),
                Err(e) => attempt.error(e),
            }
        }))
        .build()
        .expect("http client")
}

fn allow_private() -> bool {
    std::env::var("VIGIA_ALLOW_PRIVATE").is_ok_and(|v| v == "1")
}

fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => {
            let o = a.octets();
            a.is_private() || a.is_loopback() || a.is_link_local() || a.is_unspecified() || a.is_broadcast() || (o[0] == 100 && o[1] & 0xC0 == 64)
        }
        IpAddr::V6(a) => {
            let s = a.segments()[0];
            a.is_loopback() || a.is_unspecified() || s & 0xfe00 == 0xfc00 || s & 0xffc0 == 0xfe80 || a.to_ipv4_mapped().is_some_and(|m| is_private(IpAddr::V4(m)))
        }
    }
}

/// Refuse URLs that point inside the network this process runs in (other containers, the LAN).
// ponytail: resolves once here and again inside reqwest, so DNS rebinding can still slip through;
// pin the resolved address with a custom resolver if this ever faces untrusted users.
fn public_host(url: &reqwest::Url) -> Result<(), String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err("Sólo se pueden vigilar direcciones http:// o https://".into());
    }
    if allow_private() {
        return Ok(());
    }
    let host = url.host_str().ok_or("La URL no tiene host")?;
    let addrs = (host, url.port_or_known_default().unwrap_or(443))
        .to_socket_addrs()
        .map_err(|_| format!("No se pudo resolver {host}"))?;
    for a in addrs {
        if is_private(a.ip()) {
            return Err(format!("{host} apunta a una red privada"));
        }
    }
    Ok(())
}

pub async fn direct(http: &reqwest::Client, url: &str, user_agent: &str) -> Result<Page, String> {
    let parsed = reqwest::Url::parse(url).map_err(|_| "URL inválida".to_string())?;
    let check = parsed.clone();
    tokio::task::spawn_blocking(move || public_host(&check)).await.map_err(|e| e.to_string())??;

    let mut resp = http
        .get(parsed)
        .header("user-agent", user_agent)
        .header("accept", "text/html,application/xhtml+xml,application/json;q=0.9,*/*;q=0.8")
        .header("accept-language", "es-MX,es;q=0.9,en;q=0.8")
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                "La página tardó más de 30 s en responder".to_string()
            } else {
                // The redirect policy's own message is the most useful part when present.
                let mut msg = e.to_string();
                let mut src = std::error::Error::source(&e);
                while let Some(s) = src {
                    msg = s.to_string();
                    src = s.source();
                }
                format!("No se pudo conectar: {msg}")
            }
        })?;

    let status = resp.status().as_u16();
    let challenge = resp.headers().get("cf-mitigated").is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"challenge"));
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("Se cortó la descarga: {e}"))? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_BODY {
            break;
        }
    }
    // ponytail: assumes UTF-8; pages in Latin-1 lose their accents. Add encoding_rs if one shows up.
    Ok(Page {
        status,
        challenge,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

// ---------------------------------------------------------------- relay

/// A stateless fetcher on another network (see relay/worker.js) that the server calls over HTTPS.
pub struct Relay {
    pub url: String,
    pub token: String,
}

impl Relay {
    pub fn from_env() -> Option<Relay> {
        let get = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        Some(Relay {
            url: get("VIGIA_RELAY_URL")?,
            token: get("VIGIA_RELAY_TOKEN")?,
        })
    }

    pub async fn fetch(&self, http: &reqwest::Client, url: &str, user_agent: &str) -> Result<Page, String> {
        let r = http
            .post(&self.url)
            .timeout(Duration::from_secs(40))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "url": url, "user_agent": user_agent }))
            .send()
            .await
            .map_err(|e| format!("No se pudo llamar al relevo: {e}"))?;
        if !r.status().is_success() {
            return Err(format!("El relevo respondió HTTP {}", r.status().as_u16()));
        }
        r.json().await.map_err(|e| format!("Respuesta inesperada del relevo: {e}"))
    }
}

// ---------------------------------------------------------------- agents

#[derive(Serialize, Deserialize, Debug)]
pub struct Job {
    pub id: u64,
    pub url: String,
    pub user_agent: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct JobResult {
    pub id: u64,
    pub page: Option<Page>,
    pub error: Option<String>,
}

/// Fetch jobs waiting for an agent, and the callers waiting for their results.
#[derive(Default)]
pub struct Agents {
    next_id: AtomicU64,
    queue: Mutex<VecDeque<Job>>,
    waiting: Mutex<HashMap<u64, oneshot::Sender<Result<Page, String>>>>,
    /// Agent name → unix time until which we count it as connected.
    alive_until: Mutex<HashMap<String, i64>>,
    wake: Notify,
}

/// Marks the agent connected while its long-poll is parked. When the poll ends normally the agent
/// gets a short grace (it is working on a job, or about to poll again); when the connection just
/// drops, the handler is cancelled and the agent counts as gone right away.
struct Parked<'a> {
    agents: &'a Agents,
    name: &'a str,
    grace: i64,
}
impl Drop for Parked<'_> {
    fn drop(&mut self) {
        self.agents.alive_until.lock().unwrap().insert(self.name.to_string(), crate::now() + self.grace);
    }
}

impl Agents {
    /// Names of the agents connected right now.
    pub fn online(&self) -> Vec<String> {
        let now = crate::now();
        let mut v: Vec<_> = self.alive_until.lock().unwrap().iter().filter(|(_, t)| **t > now).map(|(n, _)| n.clone()).collect();
        v.sort();
        v
    }

    pub async fn fetch(&self, url: &str, user_agent: &str) -> Result<Page, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.waiting.lock().unwrap().insert(id, tx);
        self.queue.lock().unwrap().push_back(Job {
            id,
            url: url.into(),
            user_agent: user_agent.into(),
        });
        self.wake.notify_one();
        let out = tokio::time::timeout(Duration::from_secs(40), rx).await;
        self.waiting.lock().unwrap().remove(&id);
        self.queue.lock().unwrap().retain(|j| j.id != id);
        match out {
            Ok(Ok(r)) => r,
            _ => Err("El agente no respondió a tiempo".into()),
        }
    }

    /// Long-poll: the next job, or None after ~25 s.
    pub async fn next_job(&self, agent: &str) -> Option<Job> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
        self.alive_until.lock().unwrap().insert(agent.to_string(), i64::MAX);
        let mut parked = Parked {
            agents: self,
            name: agent,
            grace: 0,
        };
        loop {
            if let Some(job) = self.queue.lock().unwrap().pop_front() {
                parked.grace = 40; // busy with this job
                return Some(job);
            }
            if tokio::time::timeout_at(deadline, self.wake.notified()).await.is_err() {
                parked.grace = 5; // polls again right away
                return None;
            }
        }
    }

    pub fn finish(&self, r: JobResult) {
        if let Some(tx) = self.waiting.lock().unwrap().remove(&r.id) {
            let _ = tx.send(r.page.ok_or_else(|| r.error.unwrap_or_else(|| "El agente falló".into())));
        }
    }
}

/// `vigia agent`: runs on another network (your home, your laptop) and fetches on behalf of the server.
pub async fn run_agent() -> ! {
    let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("falta la variable {k}"));
    let server = env("VIGIA_SERVER").trim_end_matches('/').to_string();
    let token = env("VIGIA_TOKEN");
    let name = std::env::var("VIGIA_AGENT_NAME").unwrap_or_else(|_| "agente".into());
    let http = client();
    // Talks only to the server the user configured, so it may be on a private address.
    let api = reqwest::Client::builder().timeout(Duration::from_secs(40)).build().expect("http client");
    eprintln!("vigia agent «{name}» → {server}");
    loop {
        let poll = api.get(format!("{server}/api/agent/poll")).query(&[("name", &name)]).bearer_auth(&token).send().await;
        let job: Job = match poll {
            Ok(r) if r.status() == 200 => match r.json().await {
                Ok(j) => j,
                Err(_) => continue,
            },
            Ok(r) if r.status() == 204 => continue,
            Ok(r) => {
                eprintln!("el servidor respondió {}", r.status());
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
            Err(e) => {
                eprintln!("sin conexión con el servidor: {e}");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        let result = match direct(&http, &job.url, &job.user_agent).await {
            Ok(page) => JobResult {
                id: job.id,
                page: Some(page),
                error: None,
            },
            Err(e) => JobResult {
                id: job.id,
                page: None,
                error: Some(e),
            },
        };
        eprintln!("{} → {}", job.url, result.page.as_ref().map_or("error".to_string(), |p| p.status.to_string()));
        let _ = api.post(format!("{server}/api/agent/result")).bearer_auth(&token).json(&result).send().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_addresses_are_refused() {
        for bad in [
            "http://127.0.0.1/",
            "http://10.0.0.5:8080/x",
            "http://192.168.1.1/",
            "http://[::1]/",
            "http://169.254.169.254/latest",
            "ftp://example.com/",
            "http://100.64.1.1/",
        ] {
            assert!(public_host(&reqwest::Url::parse(bad).unwrap()).is_err(), "{bad}");
        }
        assert!(public_host(&reqwest::Url::parse("https://1.1.1.1/").unwrap()).is_ok());
    }
}
