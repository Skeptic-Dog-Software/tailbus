//! The API between machines: plain HTTP with JSON bodies on each machine's
//! tailnet address, which WireGuard already encrypts. Every request is
//! checked with `tailscale whois`.

use std::io::Read;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tiny_http::{Header, Method, Response, Server};

use crate::ask;
use crate::bus::Bus;
use crate::proto::{AgentList, AskAnswer, AskClose, AskShow, MessageBatch, ReceiptBatch};

/// A full batch of the largest messages, with room for JSON escaping.
const MAX_REQUEST: usize = 16 * 1024 * 1024;
const STATUS_EVERY: Duration = Duration::from_secs(15);
const PRUNE_EVERY: Duration = Duration::from_secs(3600);

pub fn http_agent(connect: Duration, overall: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(connect))
        .timeout_global(Some(overall))
        .http_status_as_error(false)
        // Tailnet addresses never go through a proxy.
        .proxy(None)
        .build()
        .into()
}

pub fn post<T: Serialize, R: DeserializeOwned>(
    http: &ureq::Agent,
    ip: Ipv4Addr,
    port: u16,
    path: &str,
    body: &T,
) -> Result<R> {
    let json = serde_json::to_vec(body)?;
    let mut resp = http
        .post(format!("http://{ip}:{port}{path}"))
        .header("Content-Type", "application/json")
        .send(&json[..])?;
    read(&mut resp)
}

/// The agents on the machine at `ip`, if its bus answers quickly.
pub fn get_agents(ip: Ipv4Addr, port: u16) -> Result<AgentList> {
    let http = http_agent(Duration::from_secs(2), Duration::from_secs(4));
    let mut resp = http.get(format!("http://{ip}:{port}/v1/agents")).call()?;
    read(&mut resp)
}

fn read<R: DeserializeOwned>(resp: &mut ureq::http::Response<ureq::Body>) -> Result<R> {
    let status = resp.status();
    let text = resp.body_mut().read_to_string()?;
    if !status.is_success() {
        let why = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v["error"].as_str().map(str::to_string))
            .unwrap_or(text);
        bail!("{status}: {}", why.trim());
    }
    Ok(serde_json::from_str(&text)?)
}

/// Listens on this machine's tailnet address, waiting for Tailscale first,
/// and retrying if the address can't be bound.
pub fn serve(bus: Arc<Bus>) {
    loop {
        let addr = match bus.me() {
            Ok(me) => match me.ipv4 {
                Some(ip) => SocketAddr::new(ip.into(), bus.cfg.port),
                None => {
                    *bus.listening.lock().unwrap() =
                        Err("this machine has no tailnet IPv4 address".into());
                    thread::sleep(Duration::from_secs(10));
                    continue;
                }
            },
            Err(e) => {
                *bus.listening.lock().unwrap() = Err(format!("{e:#}"));
                thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        match Server::http(addr) {
            Ok(server) => {
                *bus.listening.lock().unwrap() = Ok(addr);
                bus.event(format!("listening on {addr}"));
                for req in server.incoming_requests() {
                    let bus = bus.clone();
                    thread::spawn(move || handle(&bus, req));
                }
            }
            Err(e) => {
                let why = format!("can't listen on {addr}: {e}");
                bus.event(format!("{why}; retrying"));
                *bus.listening.lock().unwrap() = Err(why);
                thread::sleep(Duration::from_secs(10));
            }
        }
    }
}

fn handle(bus: &Bus, mut req: tiny_http::Request) {
    let (code, body) = match route(bus, &mut req) {
        Ok(v) => (200, v),
        Err((code, why)) => (code, json!({ "error": why })),
    };
    let header = Header::from_bytes("Content-Type", "application/json").expect("valid header");
    let resp = Response::from_string(body.to_string())
        .with_status_code(code)
        .with_header(header);
    let _ = req.respond(resp);
}

type Reject = (u16, String);

fn route(bus: &Bus, req: &mut tiny_http::Request) -> Result<Value, Reject> {
    let ip = req
        .remote_addr()
        .map(|a| a.ip())
        .ok_or((400, "no peer address".to_string()))?;
    let who = bus
        .whois(ip)
        .map_err(|e| (403, format!("{ip} isn't a known tailnet peer: {e:#}")))?;
    if !bus.allowed(&who).map_err(internal)? {
        bus.event(format!(
            "refused {} ({}): not allowed",
            who.machine, who.owner
        ));
        return Err((
            403,
            format!(
                "{} ({}) isn't allowed to use this bus; its owner can add it to `allow`",
                who.machine, who.owner
            ),
        ));
    }
    match (req.method(), req.url()) {
        (Method::Post, "/v1/messages") => {
            let batch: MessageBatch = read_json(req)?;
            let results = bus.accept(&who, batch).map_err(internal)?;
            Ok(serde_json::to_value(results).map_err(|e| internal(e.into()))?)
        }
        (Method::Post, "/v1/receipts") => {
            let batch: ReceiptBatch = read_json(req)?;
            bus.accept_receipts(&who, batch).map_err(internal)?;
            Ok(json!({}))
        }
        (Method::Get, "/v1/agents") => {
            let list = bus.agent_list().map_err(internal)?;
            Ok(serde_json::to_value(list).map_err(|e| internal(e.into()))?)
        }
        // Dialogs for the user's other Macs. `ask` refuses machines with
        // another owner, even allowed ones.
        (Method::Post, "/v1/asks") => {
            let req: AskShow = read_json(req)?;
            ask::show(bus, &who, req).map_err(|why| (409, why))?;
            Ok(json!({}))
        }
        (Method::Post, "/v1/asks/close") => {
            let req: AskClose = read_json(req)?;
            ask::closed(bus, &who, req);
            Ok(json!({}))
        }
        (Method::Post, "/v1/answers") => {
            let req: AskAnswer = read_json(req)?;
            ask::answered(bus, &who, req).map_err(|why| (404, why))?;
            Ok(json!({}))
        }
        _ => Err((404, format!("no {} {}", req.method(), req.url()))),
    }
}

fn internal(e: anyhow::Error) -> Reject {
    (500, format!("{e:#}"))
}

fn read_json<T: DeserializeOwned>(req: &mut tiny_http::Request) -> Result<T, Reject> {
    if req.body_length().is_some_and(|n| n > MAX_REQUEST) {
        return Err((413, "request too large".into()));
    }
    let mut buf = Vec::new();
    req.as_reader()
        .take(MAX_REQUEST as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| (400, e.to_string()))?;
    if buf.len() > MAX_REQUEST {
        return Err((413, "request too large".into()));
    }
    serde_json::from_slice(&buf).map_err(|e| (400, format!("bad request: {e}")))
}

/// Forwards the outbox and receipts, keeps Tailscale's view fresh, and
/// prunes old messages.
pub fn forward(bus: Arc<Bus>) {
    let http = http_agent(Duration::from_secs(3), Duration::from_secs(15));
    let mut last_status: Option<Instant> = None;
    let mut last_prune: Option<Instant> = None;
    loop {
        if last_status.is_none_or(|t| t.elapsed() >= STATUS_EVERY) {
            let _ = bus.refresh_status();
            last_status = Some(Instant::now());
        }
        if last_prune.is_none_or(|t| t.elapsed() >= PRUNE_EVERY) {
            bus.prune();
            last_prune = Some(Instant::now());
        }
        if let Err(e) = bus.forward_once(&http) {
            bus.event(format!("forwarding failed: {e:#}"));
            thread::sleep(Duration::from_secs(5));
        }
        let next = bus.db().next_due().ok().flatten();
        let wait = match next {
            Some(t) => (t - crate::util::now_ms()).clamp(0, 15_000),
            None => 15_000,
        };
        if wait > 0 {
            bus.nap(Duration::from_millis(wait as u64));
        }
    }
}
