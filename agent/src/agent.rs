//! `aiva run`: keeps an outbound WebSocket to the relay open and answers tool calls.

use crate::config::Config;
use crate::protocol::{AGENT_PROTOCOL, AgentMessage, RelayMessage};
use crate::tools;
use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::StatusCode};

/// Logged when an app asks for the tool list; `aiva setup` watches for it.
pub const APP_CONNECTED: &str = "An app connected and listed the tools";

const PING_EVERY: Duration = Duration::from_secs(30);
const DEAD_AFTER: Duration = Duration::from_secs(80);
const CLOSE_REPLACED: u16 = 4001;
const CLOSE_INVALID_SECRET: u16 = 4003;

enum End {
    /// Connection dropped; reconnect with backoff.
    Dropped(String),
    /// The relay refused this key. Wait until the config has a different key before trying again.
    Refused(String),
    /// Another connection with the same key took over. Step aside, but come back if it goes away.
    Replaced,
}

pub async fn run() -> Result<()> {
    log(&format!("aiva agent {} starting (config: {})", crate::VERSION, crate::config::config_path().display()));
    std::thread::spawn(tools::warm_up);
    let mut backoff = Duration::from_secs(1);
    let mut refused_key: Option<String> = None;
    let mut waiting_logged = false;

    loop {
        let cfg = match Config::load() {
            Ok(Some(cfg)) if cfg.key.is_some() => cfg,
            Ok(_) => {
                if !waiting_logged {
                    log("Not set up yet. Run `aiva setup`. Waiting...");
                    waiting_logged = true;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
            Err(e) => {
                log(&format!("Can't read config: {e:#}"));
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };
        if refused_key.is_some() && refused_key == cfg.key {
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        refused_key = None;
        waiting_logged = false;

        let started = Instant::now();
        match session(&cfg).await {
            End::Refused(why) => {
                log(&format!("{why} Waiting for a new key (`aiva rotate`)."));
                refused_key = cfg.key.clone();
                continue;
            }
            End::Replaced => {
                log("Another computer connected with the same key, so this one stepped aside. It takes over again if that one goes away.");
                wait_while_replaced(&cfg).await;
            }
            End::Dropped(why) => {
                if started.elapsed() > Duration::from_secs(60) {
                    backoff = Duration::from_secs(1);
                }
                log(&format!("Disconnected: {why}. Retrying in {}s.", backoff.as_secs()));
                tokio::time::sleep(backoff + jitter()).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

/// Waits while another connection holds this key: until the relay sees nobody online (that one
/// was a short check or has quit) or the key changes.
async fn wait_while_replaced(cfg: &Config) {
    loop {
        tokio::time::sleep(Duration::from_secs(20) + jitter()).await;
        if Config::load().ok().flatten().and_then(|c| c.key) != cfg.key {
            return;
        }
        let check = cfg.clone();
        let online = tokio::task::spawn_blocking(move || crate::commands::server_status(&check).ok().and_then(|s| s["online"].as_bool()))
            .await
            .ok()
            .flatten();
        if online == Some(false) {
            log("Nobody else is using this key now. Reconnecting.");
            return;
        }
    }
}

fn jitter() -> Duration {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().subsec_nanos();
    Duration::from_millis(u64::from(nanos % 1000))
}

/// The key alone identifies this computer; the relay derives its id from it.
pub fn ws_url(server: &str) -> String {
    let base = server.trim_end_matches('/');
    let base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_string()
    };
    format!("{base}/agent/connect")
}

pub fn device_name() -> String {
    hostname::get().map(|h| h.to_string_lossy().trim_end_matches(".local").to_string()).unwrap_or_else(|_| "computer".into())
}

pub fn hello() -> AgentMessage {
    AgentMessage::Hello {
        protocol: AGENT_PROTOCOL,
        version: crate::VERSION,
        device: device_name(),
        os: std::env::consts::OS,
        probe: false,
    }
}

/// Connects once to check the server accepts this key, without serving calls. Returns the id the server
/// knows this computer by; errors are phrased for people.
pub async fn verify(cfg: &Config) -> std::result::Result<String, String> {
    let ws = connect(cfg).await.map_err(|end| match end {
        End::Dropped(why) => format!("Couldn't reach {}: {why}", cfg.server),
        End::Refused(why) => why,
        End::Replaced => "Another computer took over this key.".into(),
    })?;
    let (mut sink, mut stream) = ws.split();
    let mut probe = hello();
    if let AgentMessage::Hello { probe: p, .. } = &mut probe {
        *p = true;
    }
    sink.send(Message::Text(serde_json::to_string(&probe).unwrap().into())).await.map_err(|e| e.to_string())?;
    let wait = async {
        while let Some(msg) = stream.next().await {
            match msg {
                Ok(Message::Text(text)) => match serde_json::from_str::<RelayMessage>(&text) {
                    Ok(RelayMessage::Welcome { id, .. }) => return Ok(id),
                    Ok(RelayMessage::Error { message }) => return Err(message),
                    _ => {}
                },
                Ok(Message::Close(frame)) => {
                    return Err(frame.map(|f| f.reason.to_string()).unwrap_or_else(|| "The server closed the connection.".into()));
                }
                Ok(_) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        Err("The server closed the connection.".into())
    };
    let id = tokio::time::timeout(Duration::from_secs(15), wait).await.map_err(|_| "Timed out waiting for the server.".to_string())??;
    let _ = sink.send(Message::Close(None)).await;
    Ok(id)
}

/// Opens the agent socket. Errors are already phrased for people.
async fn connect(
    cfg: &Config,
) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, End> {
    let url = ws_url(&cfg.server);
    let mut req = url.as_str().into_client_request().map_err(|e| End::Refused(format!("Bad server URL {url}: {e}.")))?;
    let secret = cfg.key.clone().unwrap_or_default();
    let headers = req.headers_mut();
    headers.insert("Authorization", format!("Bearer {secret}").parse().map_err(|_| End::Refused("The key has invalid characters.".into()))?);
    headers.insert("User-Agent", format!("aiva/{}", crate::VERSION).parse().unwrap());

    match tokio::time::timeout(Duration::from_secs(20), tokio_tungstenite::connect_async(req)).await {
        Err(_) => Err(End::Dropped(format!("timed out connecting to {}", cfg.server))),
        Ok(Ok((ws, _))) => Ok(ws),
        Ok(Err(tokio_tungstenite::tungstenite::Error::Http(resp))) => {
            let body = resp.body().as_ref().map(|b| String::from_utf8_lossy(b).trim().to_string()).unwrap_or_default();
            match resp.status() {
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(End::Refused(if body.is_empty() {
                    "The server didn't accept this key.".into()
                } else {
                    body
                })),
                status => Err(End::Dropped(format!("server answered {status}"))),
            }
        }
        Ok(Err(e)) => Err(End::Dropped(e.to_string())),
    }
}

/// One connection: connect, say hello, then serve tool calls until it drops.
async fn session(cfg: &Config) -> End {
    let ws = match connect(cfg).await {
        Ok(ws) => ws,
        Err(end) => return end,
    };
    let (mut sink, mut stream) = ws.split();
    if let Err(e) = sink.send(Message::Text(serde_json::to_string(&hello()).unwrap().into())).await {
        return End::Dropped(e.to_string());
    }

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    let mut last_heard = Instant::now();

    loop {
        tokio::select! {
            msg = stream.next() => {
                last_heard = Instant::now();
                match msg {
                    Some(Ok(Message::Text(text))) => { let received = Instant::now(); match serde_json::from_str::<RelayMessage>(&text) {
                        Ok(RelayMessage::Welcome { url, .. }) => log(&format!("Connected. Connector URL: {url}")),
                        Ok(RelayMessage::List { id }) => {
                            log(APP_CONNECTED);
                            // Read the config fresh: which tools exist depends on folder permissions.
                            let cfg = Config::load().ok().flatten().unwrap_or_default();
                            let msg = AgentMessage::Result { id, result: serde_json::json!({ "tools": tools::definitions(&cfg) }), timing: None };
                            let _ = out_tx.send(serde_json::to_string(&msg).unwrap());
                        }
                        Ok(RelayMessage::Call { id, tool, args }) => spawn_call(id, tool, args, received, out_tx.clone()),
                        Ok(RelayMessage::Error { message }) => log(&format!("Server: {message}")),
                        Ok(RelayMessage::Pong) | Ok(RelayMessage::Unknown) => {}
                        Err(e) => log(&format!("Ignoring unreadable message: {e}")),
                    } }
                    Some(Ok(Message::Close(frame))) => {
                        let (code, reason) = frame.map(|f| (u16::from(f.code), f.reason.to_string())).unwrap_or((1005, String::new()));
                        return match code {
                            CLOSE_REPLACED => End::Replaced,
                            CLOSE_INVALID_SECRET => End::Refused(format!("The server rejected this key ({reason}).")),
                            _ => End::Dropped(format!("closed by server ({code} {reason})")),
                        };
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return End::Dropped(e.to_string()),
                    None => return End::Dropped("connection closed".into()),
                }
            }
            Some(out) = out_rx.recv() => {
                if let Err(e) = sink.send(Message::Text(out.into())).await {
                    return End::Dropped(e.to_string());
                }
            }
            _ = ping.tick() => {
                if last_heard.elapsed() > DEAD_AFTER {
                    return End::Dropped("no response from server".into());
                }
                let ping = serde_json::to_string(&AgentMessage::Ping).unwrap();
                if let Err(e) = sink.send(Message::Text(ping.into())).await {
                    return End::Dropped(e.to_string());
                }
            }
        }
    }
}

fn spawn_call(id: u64, tool: String, args: serde_json::Value, received: Instant, out: mpsc::UnboundedSender<String>) {
    tokio::spawn(async move {
        let name = tool.clone();
        let (result, tool_us) = tokio::task::spawn_blocking(move || {
            let started = Instant::now();
            // Re-read the config for every call so folder changes apply immediately.
            let cfg = Config::load().ok().flatten().unwrap_or_default();
            let result = tools::call(&tool, &args, &cfg);
            (result, started.elapsed().as_micros() as u64)
        })
        .await
        .unwrap_or_else(|e| (tools::ToolError::new(format!("The tool crashed: {e}")).into(), 0));
        let flag = if result.is_error { " (error)" } else { "" };
        let mut timing = serde_json::json!({ "tool_us": tool_us, "agent_us": 0 });
        let mut msg = AgentMessage::Result { id, result: result.to_json(), timing: None };
        // agent_us covers receipt of the request → result serialized and ready to send.
        let body_len = serde_json::to_string(&msg).map(|s| s.len()).unwrap_or(0);
        timing["agent_us"] = serde_json::json!(received.elapsed().as_micros() as u64);
        timing["bytes"] = serde_json::json!(body_len);
        log(&format!("{name} tool {:.1}ms, agent {:.1}ms, {body_len} bytes{flag}", tool_us as f64 / 1000.0, timing["agent_us"].as_u64().unwrap_or(0) as f64 / 1000.0));
        if let AgentMessage::Result { timing: t, .. } = &mut msg {
            *t = Some(timing);
        }
        let _ = out.send(serde_json::to_string(&msg).unwrap());
    });
}

pub fn log(msg: &str) {
    println!("{} {msg}", timestamp());
}

/// UTC timestamp like 2026-10-12T03:57:00Z, without pulling in a date crate.
fn timestamp() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}
