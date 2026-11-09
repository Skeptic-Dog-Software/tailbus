//! Asking the user something on each of their Macs: a dialog on this one,
//! and the same dialog sent to the bus on each of their others. The first
//! answer counts, and the other dialogs close.
//!
//! Each machine an ask goes to gets a nonce that only this bus knows, kept
//! in memory. An answer counts only from that machine with that nonce, and
//! nothing on the local socket can give one, so an agent can't answer for
//! the user.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;

use crate::alert::{self, Dialog, MAX_BUTTONS};
use crate::bus::Bus;
use crate::net;
use crate::network::Whois;
use crate::proto::{AskAnswer, AskClose, AskShow};
use crate::util::{is_id, new_id};

/// How long an ask's dialogs stay up.
const DIALOG_FOR: Duration = Duration::from_secs(86_400);
/// The most text a dialog shows.
const MAX_TEXT: usize = 1500;

/// What to ask: buttons left to right, the first one the "no" one.
#[derive(Debug, Clone)]
pub struct Prompt {
    pub title: String,
    pub text: String,
    pub buttons: Vec<String>,
}

/// Which button was clicked, and on which machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub choice: usize,
    pub machine: String,
}

#[derive(Default)]
pub struct Asks {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Asks from this machine.
    out: HashMap<String, Out>,
    /// Dialogs shown here for other machines' asks.
    shown: HashMap<String, Shown>,
}

struct Out {
    /// Each other machine showing it, with the nonce its answer carries.
    targets: HashMap<String, String>,
    dialog: Option<Arc<Dialog>>,
    answer: Option<Answer>,
}

struct Shown {
    origin: String,
    dialog: Arc<Dialog>,
}

fn lock(asks: &Asks) -> std::sync::MutexGuard<'_, State> {
    asks.state.lock().unwrap_or_else(|e| e.into_inner())
}

fn http() -> ureq::Agent {
    net::http_agent(Duration::from_secs(3), Duration::from_secs(5))
}

/// Shows `prompt` on each of the user's Macs that can show it. Returns the
/// ask's ID, and how many machines it went to.
pub fn ask(bus: &Bus, prompt: Prompt) -> Result<(String, usize)> {
    let me = bus.me()?;
    let id = new_id();
    lock(&bus.asks).out.insert(
        id.clone(),
        Out {
            targets: HashMap::new(),
            dialog: None,
            answer: None,
        },
    );
    let mut sent = 0;

    if bus.cfg.alerts {
        let buttons: Vec<&str> = prompt.buttons.iter().map(String::as_str).collect();
        match Dialog::show(&prompt.title, &prompt.text, &buttons, DIALOG_FOR) {
            Ok(dialog) => {
                let dialog = Arc::new(dialog);
                if let Some(out) = lock(&bus.asks).out.get_mut(&id) {
                    out.dialog = Some(dialog.clone());
                }
                sent += 1;
                if let Some(bus) = bus.arc() {
                    let (id, machine) = (id.clone(), me.machine.clone());
                    thread::spawn(move || {
                        if let Some(choice) = dialog.answer() {
                            record(&bus, &id, choice, &machine);
                        }
                    });
                }
            }
            Err(e) => bus.event(format!("couldn't show a dialog here: {e:#}")),
        }
    }

    // The user's other Macs: same Tailscale owner only, never just allowed.
    let status = bus.status()?;
    let others: Vec<_> = status
        .peers
        .iter()
        .filter(|p| p.online && !me.owner.is_empty() && p.owner == me.owner)
        .filter_map(|p| p.ipv4.map(|ip| (p.machine.clone(), ip)))
        .collect();
    let mut asked = Vec::new();
    for (machine, ip) in others {
        let nonce = new_id();
        let show = AskShow {
            id: id.clone(),
            nonce: nonce.clone(),
            title: prompt.title.clone(),
            text: prompt.text.clone(),
            buttons: prompt.buttons.clone(),
        };
        let port = bus.cfg.port;
        asked.push((
            machine.clone(),
            nonce,
            thread::spawn(move || {
                net::post::<_, serde_json::Value>(&http(), ip, port, "/v1/asks", &show)
            }),
        ));
    }
    // Linux machines, Macs with alerts off, and buses that are down refuse
    // or don't answer, and aren't counted.
    for (machine, nonce, call) in asked {
        if let Ok(Ok(_)) = call.join() {
            if let Some(out) = lock(&bus.asks).out.get_mut(&id) {
                out.targets.insert(machine, nonce);
            }
            sent += 1;
        }
    }
    Ok((id, sent))
}

/// The answer to an ask from this machine, once there is one.
pub fn answer(bus: &Bus, id: &str) -> Option<Answer> {
    lock(&bus.asks).out.get(id).and_then(|o| o.answer.clone())
}

/// Takes an ask's dialogs down everywhere and forgets it.
pub fn close(bus: &Bus, id: &str) {
    let Some(out) = lock(&bus.asks).out.remove(id) else {
        return;
    };
    close_out(bus, id, out, None);
}

fn close_out(bus: &Bus, id: &str, out: Out, except: Option<&str>) {
    if let Some(dialog) = out.dialog {
        dialog.close();
    }
    let Ok(status) = bus.status() else { return };
    let port = bus.cfg.port;
    for machine in out.targets.keys() {
        if Some(machine.as_str()) == except {
            continue;
        }
        if let Some(ip) = status
            .peers
            .iter()
            .find(|p| &p.machine == machine)
            .and_then(|p| p.ipv4)
        {
            let close = AskClose { id: id.to_string() };
            thread::spawn(move || {
                let _ =
                    net::post::<_, serde_json::Value>(&http(), ip, port, "/v1/asks/close", &close);
            });
        }
    }
}

/// Records the first answer to an ask from this machine, and takes its
/// other dialogs down.
fn record(bus: &Bus, id: &str, choice: usize, machine: &str) {
    let others = {
        let mut state = lock(&bus.asks);
        let Some(out) = state.out.get_mut(id) else {
            return;
        };
        if out.answer.is_some() {
            return;
        }
        out.answer = Some(Answer {
            choice,
            machine: machine.to_string(),
        });
        // Keep the record for `answer`; close everything that's still up.
        Out {
            targets: out.targets.clone(),
            dialog: out.dialog.take(),
            answer: None,
        }
    };
    close_out(bus, id, others, Some(machine));
    bus.notify();
}

// The other side: asks from the user's other Macs.

/// `POST /v1/asks`: shows a dialog for another of the user's machines, and
/// sends back the answer.
pub fn show(bus: &Bus, who: &Whois, req: AskShow) -> Result<(), String> {
    let me = bus.me().map_err(|e| format!("{e:#}"))?;
    if me.owner.is_empty() || who.owner != me.owner {
        return Err(format!(
            "{} isn't owned by {}, so it can't ask {}'s user anything",
            who.machine, me.owner, me.machine
        ));
    }
    if !bus.cfg.alerts {
        return Err(format!("alerts are off on {}", me.machine));
    }
    if !is_id(&req.id) || !is_id(&req.nonce) {
        return Err("bad ask ID".into());
    }
    let buttons: Vec<String> = req
        .buttons
        .iter()
        .take(MAX_BUTTONS)
        .map(|b| alert::clean(b, 40))
        .collect();
    let refs: Vec<&str> = buttons.iter().map(String::as_str).collect();
    let dialog = Dialog::show(
        &alert::clean(&req.title, 80),
        &alert::clean(&req.text, MAX_TEXT),
        &refs,
        DIALOG_FOR,
    )
    .map(Arc::new)
    .map_err(|e| format!("{e:#}"))?;
    lock(&bus.asks).shown.insert(
        req.id.clone(),
        Shown {
            origin: who.machine.clone(),
            dialog: dialog.clone(),
        },
    );
    let Some(bus) = bus.arc() else {
        return Ok(());
    };
    let origin = who.machine.clone();
    thread::spawn(move || {
        let choice = dialog.answer();
        lock(&bus.asks).shown.remove(&req.id);
        let Some(choice) = choice else { return };
        let ip = bus.status().ok().and_then(|s| {
            s.peers
                .iter()
                .find(|p| p.machine == origin)
                .and_then(|p| p.ipv4)
        });
        if let Some(ip) = ip {
            let answer = AskAnswer {
                id: req.id,
                nonce: req.nonce,
                choice,
            };
            let _ = net::post::<_, serde_json::Value>(
                &http(),
                ip,
                bus.cfg.port,
                "/v1/answers",
                &answer,
            );
        }
    });
    Ok(())
}

/// `POST /v1/asks/close`: the machine that asked doesn't need an answer.
pub fn closed(bus: &Bus, who: &Whois, req: AskClose) {
    let mut state = lock(&bus.asks);
    if state
        .shown
        .get(&req.id)
        .is_some_and(|s| s.origin == who.machine)
        && let Some(shown) = state.shown.remove(&req.id)
    {
        drop(state);
        shown.dialog.close();
    }
}

/// `POST /v1/answers`: an answer from a machine this one asked.
pub fn answered(bus: &Bus, who: &Whois, req: AskAnswer) -> Result<(), String> {
    let valid = lock(&bus.asks)
        .out
        .get(&req.id)
        .and_then(|o| o.targets.get(&who.machine))
        .is_some_and(|nonce| *nonce == req.nonce);
    if !valid {
        return Err("no such ask for that machine".into());
    }
    record(bus, &req.id, req.choice, &who.machine);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bus() -> Arc<Bus> {
        crate::bus::tests::bus()
    }

    fn whois(machine: &str, owner: &str) -> Whois {
        Whois {
            machine: machine.into(),
            owner: owner.into(),
        }
    }

    #[test]
    fn answers_count_only_from_the_asked_machine_with_its_nonce() {
        let bus = bus();
        let id = new_id();
        lock(&bus.asks).out.insert(
            id.clone(),
            Out {
                targets: HashMap::from([("other".to_string(), "N1".to_string())]),
                dialog: None,
                answer: None,
            },
        );
        let answer = |nonce: &str, choice| AskAnswer {
            id: id.clone(),
            nonce: nonce.into(),
            choice,
        };
        assert!(answered(&bus, &whois("other", "o"), answer("WRONG", 1)).is_err());
        assert!(answered(&bus, &whois("elsewhere", "o"), answer("N1", 1)).is_err());
        assert_eq!(super::answer(&bus, &id), None);
        answered(&bus, &whois("other", "o"), answer("N1", 2)).unwrap();
        assert_eq!(
            super::answer(&bus, &id),
            Some(Answer {
                choice: 2,
                machine: "other".into()
            })
        );
        // The first answer stands.
        record(&bus, &id, 0, "here");
        assert_eq!(super::answer(&bus, &id).unwrap().choice, 2);
        close(&bus, &id);
        assert_eq!(super::answer(&bus, &id), None);
    }

    #[test]
    fn only_the_users_own_machines_can_ask() {
        let bus = bus();
        let req = AskShow {
            id: new_id(),
            nonce: new_id(),
            title: "t".into(),
            text: "x".into(),
            buttons: vec!["No".into(), "Yes".into()],
        };
        let refused = show(&bus, &whois("other", "someone-else"), req).unwrap_err();
        assert!(refused.contains("isn't owned by"), "{refused}");
    }
}
