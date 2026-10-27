//! Permission prompts as alerts: what the agent wants to do, in words a
//! person can judge at a glance, and what "Allow for session" would allow.

use serde_json::{Value, json};

use crate::alert;
use crate::ask::Prompt;

/// Tools whose permission prompt is the user answering in the terminal, a
/// question carousel or a plan to approve, rather than a call to allow.
/// They stay with the terminal: an alert must never answer them.
pub const ANSWERED_AT_TERMINAL: &[&str] = &["AskUserQuestion", "ExitPlanMode"];

pub const DENY: usize = 0;
pub const ALLOW: usize = 1;
pub const ALLOW_FOR_SESSION: usize = 2;

/// The most of a command or argument an alert shows.
const MAX_DETAIL: usize = 900;

/// `mcp__plugin_tailbus_tailbus__msg_send` -> `msg_send, from MCP server
/// plugin_tailbus_tailbus`.
fn tool_name(tool: &str) -> String {
    match tool.strip_prefix("mcp__").and_then(|t| t.rsplit_once("__")) {
        Some((server, name)) => format!("{name}, from MCP server {server}"),
        None => tool.to_string(),
    }
}

fn field<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str)
}

/// What the call does: Claude's description, then the command, path or
/// arguments themselves.
pub fn describe(tool: &str, input: &Value) -> String {
    let mut out = Vec::new();
    if let Some(d) = field(input, "description") {
        out.push(alert::clean(d, 200));
    }
    let detail = match tool {
        "Bash" => field(input, "command").map(|c| format!("$ {c}")),
        "Write" => field(input, "file_path").map(|p| {
            let lines = field(input, "content").map_or(0, |c| c.lines().count());
            format!("{p} ({lines} lines)")
        }),
        "Edit" | "MultiEdit" | "Read" => field(input, "file_path").map(str::to_string),
        "NotebookEdit" => field(input, "notebook_path").map(str::to_string),
        "WebFetch" => field(input, "url").map(str::to_string),
        "WebSearch" => field(input, "query").map(str::to_string),
        _ => None,
    };
    let detail = detail.unwrap_or_else(|| {
        let mut args = input.clone();
        if let Some(map) = args.as_object_mut() {
            map.remove("description");
        }
        serde_json::to_string_pretty(&args).unwrap_or_default()
    });
    out.push(alert::clean(&detail, MAX_DETAIL));
    out.join("\n")
}

/// The rules "Allow for session" adds, until the session ends: Claude
/// Code's own suggested allow rules for the call, or failing those, this
/// exact command for Bash and the whole tool for anything else. Suggested
/// mode changes, such as accepting all edits, are left out, as are
/// directory grants, so the button never allows more than it says.
pub fn session_rules(tool: &str, input: &Value, suggestions: &Value) -> Vec<Value> {
    let mut rules: Vec<Value> = suggestions
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["type"] == "addRules" && s["behavior"] == "allow")
        .flat_map(|s| s["rules"].as_array().cloned().unwrap_or_default())
        .collect();
    if rules.is_empty() {
        rules.push(match (tool, field(input, "command")) {
            ("Bash", Some(command)) => json!({"toolName": "Bash", "ruleContent": command}),
            ("Bash", None) => return Vec::new(),
            _ => json!({"toolName": tool}),
        });
    }
    vec![json!({
        "type": "addRules",
        "rules": rules,
        "behavior": "allow",
        "destination": "session",
    })]
}

/// `Bash(npm test)`, `Write`.
fn rules_text(rules: &[Value]) -> String {
    rules
        .iter()
        .flat_map(|u| u["rules"].as_array().cloned().unwrap_or_default())
        .map(|r| {
            let tool = r["toolName"].as_str().unwrap_or("?");
            match r["ruleContent"].as_str() {
                Some(content) => format!("{tool}({content})"),
                None => format!("{tool}, for any call"),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub struct Request<'a> {
    /// `name@machine`.
    pub agent: &'a str,
    pub tool: &'a str,
    pub input: &'a Value,
    pub cwd: &'a str,
    /// The agent whose message started the turn, if one did.
    pub woken_by: Option<&'a str>,
    pub rules: &'a [Value],
}

pub fn prompt(r: &Request) -> Prompt {
    let mut text = format!("{} wants to use {}.", r.agent, tool_name(r.tool));
    if let Some(by) = r.woken_by {
        text.push_str(&format!(" A message from {by} started this turn."));
    }
    text.push_str("\n\n");
    text.push_str(&describe(r.tool, r.input));
    if !r.cwd.is_empty() {
        text.push_str(&format!("\n\nIn {}.", alert::clean(r.cwd, 200)));
    }
    let mut buttons = vec!["Deny".to_string(), "Allow".to_string()];
    if !r.rules.is_empty() {
        text.push_str(&format!(
            "\n\nAllow for session also allows, until that session ends: {}.",
            alert::clean(&rules_text(r.rules), 400)
        ));
        buttons.push("Allow for session".into());
    }
    Prompt {
        title: "tailbus: permission".into(),
        text,
        buttons,
    }
}

/// The `PermissionRequest` hook's output for a choice from the alert.
pub fn decision(choice: usize, machine: &str, rules: &[Value]) -> Value {
    let decision = match choice {
        ALLOW => json!({"behavior": "allow"}),
        ALLOW_FOR_SESSION => json!({"behavior": "allow", "updatedPermissions": rules}),
        _ => json!({
            "behavior": "deny",
            "message": format!("Your user denied this from the tailbus alert on {machine}."),
        }),
    };
    json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": decision}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_calls_in_plain_terms() {
        let bash = json!({"command": "rm -rf node_modules", "description": "Remove node_modules"});
        assert_eq!(
            describe("Bash", &bash),
            "Remove node_modules\n$ rm -rf node_modules"
        );
        let write = json!({"file_path": "/tmp/a.txt", "content": "one\ntwo\n"});
        assert_eq!(describe("Write", &write), "/tmp/a.txt (2 lines)");
        let mcp = json!({"to": "sim@server", "body": "hi"});
        assert!(describe("mcp__x__msg_send", &mcp).contains("\"to\": \"sim@server\""));
        assert_eq!(
            tool_name("mcp__plugin_tailbus_tailbus__msg_send"),
            "msg_send, from MCP server plugin_tailbus_tailbus"
        );
    }

    #[test]
    fn session_rules_never_change_the_mode() {
        let bash = json!({"command": "npm test"});
        // What Claude Code suggested for a `touch`: a directory and a mode.
        let suggested = json!([
            {"type": "addDirectories", "directories": ["/x"], "destination": "session"},
            {"type": "setMode", "mode": "acceptEdits", "destination": "session"}
        ]);
        let rules = session_rules("Bash", &bash, &suggested);
        assert_eq!(rules_text(&rules), "Bash(npm test)");
        assert_eq!(rules[0]["destination"], "session");

        let suggested = json!([{"type": "addRules", "behavior": "allow", "destination": "localSettings",
                                "rules": [{"toolName": "Bash", "ruleContent": "npm run *"}]}]);
        let rules = session_rules("Bash", &bash, &suggested);
        assert_eq!(rules_text(&rules), "Bash(npm run *)");
        assert_eq!(rules[0]["destination"], "session");

        assert_eq!(
            rules_text(&session_rules("Write", &json!({}), &Value::Null)),
            "Write, for any call"
        );
    }

    #[test]
    fn prompts_say_who_asks_and_what_allow_for_session_covers() {
        let input = json!({"command": "touch marker.txt"});
        let rules = session_rules("Bash", &input, &Value::Null);
        let p = prompt(&Request {
            agent: "work@server",
            tool: "Bash",
            input: &input,
            cwd: "/Users/dana/Work",
            woken_by: Some("skeptic@laptop"),
            rules: &rules,
        });
        assert!(p.text.starts_with("work@server wants to use Bash."));
        assert!(
            p.text
                .contains("A message from skeptic@laptop started this turn.")
        );
        assert!(p.text.contains("$ touch marker.txt"));
        assert!(
            p.text
                .contains("until that session ends: Bash(touch marker.txt).")
        );
        assert_eq!(p.buttons, ["Deny", "Allow", "Allow for session"]);
    }

    #[test]
    fn decisions_match_the_hook_format() {
        let rules = session_rules("Bash", &json!({"command": "ls"}), &Value::Null);
        let allow = decision(ALLOW, "laptop", &rules);
        assert_eq!(
            allow["hookSpecificOutput"]["decision"],
            json!({"behavior": "allow"})
        );
        let session = decision(ALLOW_FOR_SESSION, "laptop", &rules);
        assert_eq!(
            session["hookSpecificOutput"]["decision"]["updatedPermissions"][0]["destination"],
            "session"
        );
        let deny = decision(DENY, "laptop", &rules);
        assert_eq!(deny["hookSpecificOutput"]["decision"]["behavior"], "deny");
        assert_eq!(
            deny["hookSpecificOutput"]["hookEventName"],
            "PermissionRequest"
        );
    }
}
