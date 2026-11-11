//! The API between machines: plain HTTP with JSON bodies on each machine's
//! tailnet address, which WireGuard already encrypts. Every request is
//! checked with `tailscale whois`.

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tiny_http::{Header, Method, Response, Server};

use crate::ask;
use crate::bus::Bus;
use crate::config::APP_NAME;
use crate::proto::{
    AgentList, AskAnswer, AskClose, AskShow, Attachment, MessageBatch, ReceiptBatch,
};

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

/// A bus answered with an error.
#[derive(Debug)]
pub struct HttpError {
    pub code: u16,
    pub why: String,
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.why)
    }
}

impl std::error::Error for HttpError {}

impl HttpError {
    /// Trying again won't change the answer.
    pub fn is_final(&self) -> bool {
        matches!(self.code, 400 | 403 | 404 | 411 | 413 | 422)
    }

    /// What it means, for whoever sent the message.
    pub fn describe(&self, machine: &str) -> String {
        match self.code {
            404 => format!(
                "the bus on {machine} is too old for options or attachments; update its {APP_NAME}"
            ),
            _ => format!("{machine} refused it: {}", self.why),
        }
    }
}

/// Copies an attachment to the bus at `ip`, unless it's there already.
pub fn upload(
    http: &ureq::Agent,
    ip: Ipv4Addr,
    port: u16,
    path: &Path,
    a: &Attachment,
) -> Result<()> {
    let url = format!("http://{ip}:{port}/v2/attachments/{}", a.sha256);
    let mut resp = http.get(&url).call()?;
    let have: Value = read(&mut resp)?;
    if have["size"].as_u64() == Some(a.size) {
        return Ok(());
    }
    let mut file = File::open(path)?;
    let mut resp = http
        .put(&url)
        .header("Content-Length", a.size.to_string())
        .send(ureq::SendBody::from_reader(&mut file))?;
    read::<Value>(&mut resp).map(|_| ())
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
        return Err(HttpError {
            code: status.as_u16(),
            why: why.trim().to_string(),
        }
        .into());
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
                answer(&bus, &server);
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

/// Answers requests, each on its own thread, until the server stops.
fn answer(bus: &Arc<Bus>, server: &Server) {
    for req in server.incoming_requests() {
        let bus = bus.clone();
        thread::spawn(move || handle(&bus, req));
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
    if let Some(sha256) = req.url().strip_prefix("/v2/attachments/") {
        let sha256 = sha256.to_string();
        return attachment(bus, req, &sha256);
    }
    match (req.method(), req.url()) {
        // The same, for messages that mean something else if their extras
        // are dropped: a bus that would drop them has no such route.
        (Method::Post, "/v1/messages" | "/v2/messages") => {
            let batch: MessageBatch = read_json(req)?;
            let results = bus.accept(&who, batch).map_err(internal)?;
            Ok(serde_json::to_value(results).map_err(|e| internal(e.into()))?)
        }
        (Method::Post, "/v1/receipts" | "/v2/receipts") => {
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

/// Says how much of an attachment is here, or takes one.
fn attachment(bus: &Bus, req: &mut tiny_http::Request, sha256: &str) -> Result<Value, Reject> {
    match req.method() {
        Method::Get => {
            let path = bus.attachments.path(sha256);
            let size = crate::attach::is_sha256(sha256)
                .then(|| std::fs::metadata(path).map(|m| m.len()).ok())
                .flatten();
            Ok(json!({ "size": size }))
        }
        Method::Put => {
            let Some(size) = req.body_length() else {
                return Err((411, "an attachment's size must be known".into()));
            };
            bus.attachments
                .receive(sha256, size as u64, req.as_reader())
                .map_err(|r| (r.code, r.why))?;
            bus.event(format!("received attachment {sha256} ({size} bytes)"));
            Ok(json!({}))
        }
        _ => Err((405, format!("no {} on attachments", req.method()))),
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

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::bus::Draft;
    use crate::bus::tests::bus_on;
    use crate::local::Item;
    use crate::proto::{Extras, MAX_BODY, RunOptions, RunResult, RunStatus, SendState};
    use crate::util::new_id;

    const LOOPBACK: Ipv4Addr = Ipv4Addr::LOCALHOST;

    fn until<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(value) = check() {
                return value;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn keep_forwarding(bus: &Arc<Bus>) {
        let bus = bus.clone();
        thread::spawn(move || {
            let http = http_agent(Duration::from_secs(3), Duration::from_secs(15));
            while Arc::strong_count(&bus) > 1 {
                let _ = bus.forward_once(&http);
                bus.nap(Duration::from_millis(50));
            }
        });
    }

    /// Two buses that know each other, talking real HTTP on this machine:
    /// `here`, and `there`, which has an agent `api`.
    fn two_machines(there_config: &str) -> (Arc<Bus>, Arc<Bus>) {
        let server = Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let there = bus_on("there", port, there_config);
        there.db().add_agent("api", 1).unwrap();
        there.pretend_peer("here", LOOPBACK);
        {
            let there = there.clone();
            thread::spawn(move || answer(&there, &server));
        }
        let here = bus_on("here", port, "");
        here.db().add_agent("sim", 1).unwrap();
        here.pretend_peer("there", LOOPBACK);
        keep_forwarding(&here);
        (here, there)
    }

    fn with_file(bus: &Bus, name: &str, bytes: &[u8]) -> Option<Box<Extras>> {
        let a = bus
            .attachments
            .add(name, &mut &bytes[..], bytes.len() as u64)
            .unwrap();
        Some(Box::new(Extras {
            attachments: vec![a],
            ..Extras::default()
        }))
    }

    fn send(here: &Bus, ext: Option<Box<Extras>>) -> Result<crate::local::Sent> {
        let draft = Draft {
            ext,
            ..Draft::default()
        };
        here.send_draft("sim", "api@there", "look at this".into(), draft)
    }

    #[test]
    fn an_attachment_crosses_to_another_machine_as_a_local_file() {
        let (here, there) = two_machines("");
        let bytes = vec![7u8; 3 * 1024 * 1024];
        let ext = with_file(&here, "data.bin", &bytes);
        let sent = send(&here, ext).unwrap();
        let found = until("the message", || {
            there.db().open_incoming("api").ok()?.into_iter().next()
        });
        assert_eq!(found.msg.id, sent.id);
        let mut item = Item::from(found);
        there.locate(&mut item);
        let a = &item.msg.attachments()[0];
        assert_eq!(a.name, "data.bin");
        let path = a.path.as_ref().expect("a path on this machine");
        assert_eq!(fs::read(path).unwrap(), bytes);
        assert!(path.ends_with(&a.sha256));
    }

    #[test]
    fn a_file_already_there_isnt_sent_again() {
        let (here, there) = two_machines("");
        let ext = with_file(&here, "a.txt", b"same words");
        send(&here, ext.clone()).unwrap();
        until("the first", || {
            let n = there.db().open_incoming("api").ok()?.len();
            (n == 1).then_some(())
        });
        // Gone from the sender's store, the second send has only the hash
        // to go on, so it can only arrive if nothing's uploaded.
        let a = ext.as_ref().unwrap().attachments[0].clone();
        let body = there.attachments.path(&a.sha256);
        assert!(body.exists());
        send(&here, ext).unwrap();
        until("the second", || {
            let n = there.db().open_incoming("api").ok()?.len();
            (n == 2).then_some(())
        });
    }

    #[test]
    fn a_bad_hash_stops_the_message_with_a_clear_error() {
        let (here, there) = two_machines("");
        let ext = with_file(&here, "a.txt", b"the real words");
        let a = ext.as_ref().unwrap().attachments[0].clone();
        // It changes after it's hashed, at the same size.
        fs::write(here.attachments.path(&a.sha256), b"the fake words").unwrap();
        let err = send(&here, ext).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("hash"), "{text}");
        assert!(there.db().open_incoming("api").unwrap().is_empty());
        assert!(!there.attachments.path(&a.sha256).exists());
    }

    #[test]
    fn a_message_naming_a_file_that_never_came_is_refused() {
        let (_here, there) = two_machines("");
        let who = crate::network::Whois {
            machine: "here".into(),
            owner: "o".into(),
        };
        let ext = Extras {
            attachments: vec![crate::proto::Attachment {
                name: "../../etc/passwd".into(),
                size: 5,
                sha256: "0".repeat(64),
                path: Some("/etc/passwd".into()),
            }],
            ..Extras::default()
        };
        let batch = MessageBatch {
            messages: vec![crate::proto::Message {
                id: new_id(),
                from: "sim@here".into(),
                to: "api@there".into(),
                body: "hello".into(),
                reply_to: None,
                expects_reply: true,
                created_at: 1,
                pipe: None,
                attended: false,
                ext: Some(Box::new(ext)),
            }],
        };
        let results = there.accept(&who, batch).unwrap();
        assert_ne!(results.results[0].status, crate::proto::StoreStatus::Stored);
        assert!(there.db().open_incoming("api").unwrap().is_empty());
    }

    #[test]
    fn a_path_from_the_sender_is_never_believed() {
        let (_here, there) = two_machines("");
        let a = there
            .attachments
            .add("a.txt", &mut &b"words"[..], 5)
            .unwrap();
        let who = crate::network::Whois {
            machine: "here".into(),
            owner: "o".into(),
        };
        let mut forged = a.clone();
        forged.path = Some("/etc/passwd".into());
        let batch = MessageBatch {
            messages: vec![crate::proto::Message {
                id: new_id(),
                from: "sim@here".into(),
                to: "api@there".into(),
                body: "hello".into(),
                reply_to: None,
                expects_reply: true,
                created_at: 1,
                pipe: None,
                attended: false,
                ext: Some(Box::new(Extras {
                    attachments: vec![forged],
                    ..Extras::default()
                })),
            }],
        };
        there.accept(&who, batch).unwrap();
        let stored = there.db().open_incoming("api").unwrap();
        assert_eq!(stored[0].msg.attachments()[0].path, None);
    }

    #[test]
    fn an_attachment_over_the_receivers_maximum_is_a_clear_error() {
        let (here, there) = two_machines("max_attachment_bytes = 10");
        let ext = with_file(&here, "big.txt", &[b'x'; 100]);
        let err = send(&here, ext).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("there refused it"), "{text}");
        assert!(text.contains("max_attachment_bytes"), "{text}");
        assert!(there.db().open_incoming("api").unwrap().is_empty());
    }

    /// A bus from before attachments: no `/v2` routes, and it keeps what
    /// reaches `/v1/messages`.
    fn old_bus(here_port: &Mutex<u16>) -> (u16, Arc<Mutex<Vec<crate::proto::Message>>>) {
        let server = Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        *here_port.lock().unwrap() = port;
        let got = Arc::new(Mutex::new(Vec::new()));
        let kept = got.clone();
        thread::spawn(move || {
            for mut req in server.incoming_requests() {
                if req.url() != "/v1/messages" {
                    let _ = req.respond(Response::from_string("{}").with_status_code(404));
                    continue;
                }
                let batch: MessageBatch = read_json(&mut req).unwrap();
                let results: Vec<_> = batch
                    .messages
                    .iter()
                    .map(|m| json!({"id": m.id, "status": "stored"}))
                    .collect();
                kept.lock().unwrap().extend(batch.messages);
                let _ = req.respond(Response::from_string(
                    json!({"results": results}).to_string(),
                ));
            }
        });
        (port, got)
    }

    use std::sync::Mutex;

    #[test]
    fn options_for_an_old_bus_fail_rather_than_being_dropped() {
        let port = Mutex::new(0);
        let (port, got) = old_bus(&port);
        let here = bus_on("here", port, "");
        here.db().add_agent("sim", 1).unwrap();
        here.pretend_peer("there", LOOPBACK);
        keep_forwarding(&here);
        let ext = Extras {
            options: Some(RunOptions {
                model: Some("m".into()),
                ..RunOptions::default()
            }),
            ..Extras::default()
        };
        let err = send(&here, Some(Box::new(ext))).unwrap_err();
        assert!(format!("{err:#}").contains("too old"), "{err:#}");
        assert!(got.lock().unwrap().is_empty());
    }

    #[test]
    fn a_large_reply_to_an_old_bus_arrives_cut_with_a_note() {
        let port = Mutex::new(0);
        let (port, got) = old_bus(&port);
        let here = bus_on("here", port, "");
        here.db().add_agent("api", 1).unwrap();
        here.pretend_peer("there", LOOPBACK);
        keep_forwarding(&here);
        let mut ext = with_file(&here, "reply.txt", b"the whole reply").unwrap();
        ext.result = Some(RunResult {
            status: RunStatus::Ok,
            run_id: new_id(),
            exit_code: Some(0),
            duration_ms: 1,
            stderr_tail: None,
            applied: None,
            full_reply: Some(ext.attachments[0].sha256.clone()),
            usage: None,
        });
        let draft = Draft {
            reply_to: Some(new_id()),
            ext: Some(ext),
            ..Draft::default()
        };
        let sent = here
            .send_draft("api", "sim@there", "the start of it".into(), draft)
            .unwrap();
        let reply = until("the reply", || got.lock().unwrap().first().cloned());
        assert_eq!(reply.id, sent.id);
        assert!(reply.body.starts_with("the start of it"), "{}", reply.body);
        assert!(reply.body.contains("too old"), "{}", reply.body);
        assert!(reply.body.len() <= MAX_BODY);
        assert!(reply.attachments().is_empty());
        assert_eq!(reply.result().unwrap().full_reply, None);
        until("it to count as sent", || {
            let state = here.db().outgoing(&sent.id).ok()??.state;
            (state == SendState::Stored).then_some(())
        });
    }

    #[test]
    fn attachments_go_after_their_retention_unless_a_message_needs_them() {
        let bus = bus_on("here", 1, "attachment_retention_days = 0");
        bus.db().add_agent("api", 1).unwrap();
        let needed = with_file(&bus, "needed.txt", b"needed");
        let old = bus.attachments.add("old.txt", &mut &b"old"[..], 3).unwrap();
        let msg = crate::proto::Message {
            id: new_id(),
            from: "sim@there".into(),
            to: "api@here".into(),
            body: "hello".into(),
            reply_to: None,
            expects_reply: true,
            created_at: 1,
            pipe: None,
            attended: false,
            ext: needed.clone(),
        };
        // Stored now: one stored long ago would be stale, and need nothing.
        bus.db()
            .store_incoming(&msg, "o", crate::util::now_ms())
            .unwrap();
        bus.prune();
        assert!(!bus.attachments.has(&old));
        assert!(bus.attachments.has(&needed.unwrap().attachments[0]));
    }
}
