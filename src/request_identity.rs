use std::borrow::Cow;
use std::fmt::Write as _;

use http::HeaderMap;
use sha2::{Digest, Sha256};

pub const CLAUDE_SESSION_HEADER: &str = "x-claude-code-session-id";
pub const CLAUDE_AGENT_HEADER: &str = "x-claude-code-agent-id";
pub const CLAUDE_PARENT_AGENT_HEADER: &str = "x-claude-code-parent-agent-id";

const MAX_IDENTITY_LEN: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ConversationIdentity {
    Main(String),
    Agent(String, String),
}

impl ConversationIdentity {
    pub fn from_headers(headers: &HeaderMap) -> Option<Self> {
        let session = read_identity_header(headers, CLAUDE_SESSION_HEADER);
        let agent = read_identity_header(headers, CLAUDE_AGENT_HEADER);
        let parent = read_identity_header(headers, CLAUDE_PARENT_AGENT_HEADER);

        if session.is_invalid() || agent.is_invalid() || parent.is_invalid() {
            return None;
        }

        match (session.value(), agent.value(), parent.value()) {
            (Some(session_id), Some(agent_id), _) => {
                Some(Self::Agent(session_id.to_string(), agent_id.to_string()))
            }
            (Some(session_id), None, None) => Some(Self::Main(session_id.to_string())),
            _ => None,
        }
    }

    pub fn search_session_id(&self) -> Cow<'_, str> {
        match self {
            Self::Main(session_id) => Cow::Borrowed(session_id),
            Self::Agent(session_id, agent_id) => {
                let mut digest = Sha256::new();
                digest.update(b"claude-code-proxy:search-agent:v1\0");
                for value in [session_id, agent_id] {
                    digest.update((value.len() as u64).to_be_bytes());
                    digest.update(value.as_bytes());
                }
                let digest = digest.finalize();
                let mut encoded = String::with_capacity(digest.len() * 2);
                for byte in digest {
                    write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
                }
                Cow::Owned(format!("search-agent-v1-{encoded}"))
            }
        }
    }
}

#[derive(Debug)]
enum ParsedHeader<'a> {
    Missing,
    Valid(&'a str),
    Invalid,
}

impl ParsedHeader<'_> {
    fn value(&self) -> Option<&str> {
        match self {
            Self::Valid(value) => Some(value),
            Self::Missing | Self::Invalid => None,
        }
    }

    fn is_invalid(&self) -> bool {
        matches!(self, Self::Invalid)
    }
}

fn read_identity_header<'a>(headers: &'a HeaderMap, name: &str) -> ParsedHeader<'a> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return ParsedHeader::Missing;
    };
    if values.next().is_some() {
        return ParsedHeader::Invalid;
    }

    let Ok(value) = value.to_str() else {
        return ParsedHeader::Invalid;
    };
    let value = value.trim_matches(|character| matches!(character, ' ' | '\t'));
    if value.is_empty()
        || value.len() > MAX_IDENTITY_LEN
        || value.contains(',')
        || !value.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return ParsedHeader::Invalid;
    }

    ParsedHeader::Valid(value)
}
