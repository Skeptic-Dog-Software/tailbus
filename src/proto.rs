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
    /// Set on the messages of a `/pipe`: which step of it this is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipe: Option<Pipe>,
    /// Sent with the sender's user in the loop, as the sending bus saw it.
    /// A receiving bus believes it only from its owner's own machines.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub attended: bool,
}

/// A `/pipe` message: a task handed to a worker, the worker's clarifying
/// questions and the user's answers, its plan and the user's approval, and
/// the worker's result. The hub can cancel the task at any point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pipe {
    pub stage: PipeStage,
    /// A task's model and effort for the remote work, if not the worker's
    /// own model and `xhigh`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// A task's cap on subagents running at once, from the hub's config. The
    /// hub's bus sets it, not the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_subagents: Option<u32>,
    /// The ID of the task a plan, an approval, a cancel or a result belongs
    /// to. The sending bus sets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// A plan's contents, with the estimate the worker's bus made of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<Box<Plan>>,
    /// On an approval, the tokens the task may use in all. On a "smaller
    /// plan" answer, `None`. The hub's bus sets it from the plan, not the
    /// agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved: Option<u64>,
    /// On a result, the tokens the task used, as tailbus counted them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<Used>,
}

impl Pipe {
    pub fn new(stage: PipeStage) -> Pipe {
        Pipe {
            stage,
            model: None,
            effort: None,
            max_subagents: None,
            task: None,
            plan: None,
            approved: None,
            used: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PipeStage {
    Task,
    Questions,
    Answers,
    Plan,
    Approval,
    Cancel,
    Result,
}

impl PipeStage {
    /// As it's serialized.
    pub fn as_str(self) -> &'static str {
        match self {
            PipeStage::Task => "task",
            PipeStage::Questions => "questions",
            PipeStage::Answers => "answers",
            PipeStage::Plan => "plan",
            PipeStage::Approval => "approval",
            PipeStage::Cancel => "cancel",
            PipeStage::Result => "result",
        }
    }
}

/// What a worker means to run for a task, or for what's left of one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// Subagents that research and build.
    pub researchers: u32,
    pub builders: u32,
    /// The ACR dimensions, one reviewer each per round.
    pub dimensions: Vec<String>,
    /// ACR's round cap, 1 to 3.
    pub rounds: u32,
    pub model: String,
    pub effort: String,
    /// What the task had used when the plan was made, counted by tailbus.
    #[serde(default)]
    pub used_so_far: u64,
    /// Set by the worker's bus, not the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate: Option<Estimate>,
}

/// A plan's tokens, from what earlier tasks used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Estimate {
    /// Every dimension confirmed in its first round.
    pub low: u64,
    /// As many rounds as earlier tasks took.
    pub expected: u64,
    /// Every dimension through every round and a verify-only round.
    pub worst: u64,
    /// What approving it allows the task in all: the worst case plus what
    /// the task had already used.
    pub budget: u64,
    /// What the figures come from.
    pub basis: String,
}

/// The tokens a task used, split as `tailbus usage` splits them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Used {
    pub session: u64,
    pub subagents: u64,
    pub subagent_count: u32,
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
    /// The client its live session is a worker for: tagged for that client,
    /// and billed to an API key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// How its live session is billed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing: Option<Billing>,
    /// Why its session's client tag didn't hold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag_refused: Option<String>,
}

/// How a session is billed, as `claude auth status` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Billing {
    /// `claude.ai`, `oauth_token`, `api_key`, `api_key_helper`,
    /// `third_party` or `none`.
    pub method: String,
    /// The subscription, on `claude.ai`, such as `max`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
}

impl Billing {
    /// Billed to an API key: Anthropic's, one from a helper script, or a
    /// cloud provider's. Not to a subscription.
    pub fn api_key(&self) -> bool {
        matches!(
            self.method.as_str(),
            "api_key" | "api_key_helper" | "third_party"
        )
    }
}

impl std::fmt::Display for Billing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.plan {
            Some(plan) => write!(f, "{} ({plan})", self.method),
            None => write!(f, "{}", self.method),
        }
    }
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
