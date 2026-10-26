//! What travels between machines: messages, receipts, and the bodies of the
//! HTTP API they ride on.

use serde::{Deserialize, Serialize};

/// The largest message body, in bytes.
pub const MAX_BODY: usize = 64 * 1024;
/// The most messages or receipts in one request.
pub const MAX_BATCH: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// A ULID.
    pub id: String,
    /// The sending agent, `name@machine`.
    pub from: String,
    /// The receiving agent, `name@machine`.
    pub to: String,
    pub body: String,
    /// The ID of the message this one answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    /// The sender is waiting on an answer.
    #[serde(default)]
    pub expects_reply: bool,
    /// Unix milliseconds.
    pub created_at: i64,
}

impl Message {
    /// What it is to its recipient: a request asks for an answer, while a
    /// response (to one of the recipient's messages) or a notice needs
    /// nothing back.
    pub fn kind(&self) -> &'static str {
        if self.expects_reply {
            "request"
        } else if self.reply_to.is_some() {
            "response"
        } else {
            "notice"
        }
    }
}

/// How far a sent message has got. States only move forward, except that
/// any state can become `Failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SendState {
    /// Still in this machine's outbox.
    Queued,
    /// The recipient's machine has it.
    Stored,
    /// A session of the recipient took it.
    Delivered,
    /// The recipient replied to it or marked it handled.
    Acknowledged,
    /// It can't be delivered, for example because there's no such agent.
    Failed,
}

impl SendState {
    pub fn as_str(self) -> &'static str {
        match self {
            SendState::Queued => "queued",
            SendState::Stored => "stored",
            SendState::Delivered => "delivered",
            SendState::Acknowledged => "acknowledged",
            SendState::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> SendState {
        match s {
            "queued" => SendState::Queued,
            "stored" => SendState::Stored,
            "delivered" => SendState::Delivered,
            "acknowledged" => SendState::Acknowledged,
            _ => SendState::Failed,
        }
    }
}

/// Tells a message's sender that it was delivered or acknowledged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub id: String,
    pub state: SendState,
    pub at: i64,
}

/// `POST /v1/messages`.
#[derive(Serialize, Deserialize)]
pub struct MessageBatch {
    pub messages: Vec<Message>,
}

/// The reply to `POST /v1/messages`: one result per message.
#[derive(Serialize, Deserialize)]
pub struct StoreResults {
    pub results: Vec<StoreResult>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StoreResult {
    pub id: String,
    pub status: StoreStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Whether the recipient has a live session, when stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live: Option<bool>,
    /// When the recipient last had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_live: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreStatus {
    /// Stored, or already stored before: the sender can stop retrying.
    Stored,
    UnknownAgent,
    /// Refused for good, for example for being too large.
    Rejected,
}

/// `POST /v1/receipts`.
#[derive(Serialize, Deserialize)]
pub struct ReceiptBatch {
    pub receipts: Vec<Receipt>,
}

/// `GET /v1/agents`, and the local `agent list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentList {
    pub machine: String,
    pub agents: Vec<AgentInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub name: String,
    /// A session holds the name right now.
    pub live: bool,
    /// The agent app of its latest session, as that app names itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// When a session last held the name, in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_live: Option<i64>,
}

/// `POST /v1/asks`: show the user a dialog for another of their machines.
/// The answer goes back with `nonce`, which only the asking bus knows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskShow {
    pub id: String,
    pub nonce: String,
    pub title: String,
    pub text: String,
    /// Left to right; the first is the "no" button.
    pub buttons: Vec<String>,
}

/// `POST /v1/answers`: which button the user clicked.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskAnswer {
    pub id: String,
    pub nonce: String,
    pub choice: usize,
}

/// `POST /v1/asks/close`: the ask was answered elsewhere, or withdrawn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskClose {
    pub id: String,
}
