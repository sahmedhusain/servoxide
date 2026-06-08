//! Cookie parsing and an in-memory session store.
//!
//! Each connection that arrives without a valid `session_id` cookie is issued
//! one (128 random bits, hex-encoded) via `Set-Cookie`. Sessions live in a
//! `HashMap` and are evicted after a TTL so the map cannot grow without bound.

use std::collections::HashMap;
use std::io::Read;
use std::time::{Duration, Instant};

const SESSION_COOKIE: &str = "session_id";

/// How long a session survives with no requests.
const SESSION_TTL: Duration = Duration::from_secs(30 * 60);

struct Session {
    visits: u64,
    last_seen: Instant,
}

/// Outcome of touching the store for one request.
pub struct SessionInfo {
    pub id: String,
    /// True if a fresh session was created (caller should `Set-Cookie`).
    pub is_new: bool,
    pub visits: u64,
}

pub struct SessionStore {
    sessions: HashMap<String, Session>,
}

impl SessionStore {
    pub fn new() -> Self {
        SessionStore {
            sessions: HashMap::new(),
        }
    }

    /// Look up (or create) the session named by the request's `Cookie` header,
    /// counting this visit. Expired sessions are evicted first.
    pub fn touch(&mut self, cookie_header: Option<&str>) -> SessionInfo {
        let now = Instant::now();
        self.evict_expired(now);

        let existing = cookie_header
            .and_then(|h| cookie_value(h, SESSION_COOKIE))
            .filter(|id| self.sessions.contains_key(id));

        match existing {
            Some(id) => {
                let s = self.sessions.get_mut(&id).unwrap();
                s.visits += 1;
                s.last_seen = now;
                SessionInfo {
                    visits: s.visits,
                    id,
                    is_new: false,
                }
            }
            None => {
                let id = random_id();
                self.sessions.insert(
                    id.clone(),
                    Session {
                        visits: 1,
                        last_seen: now,
                    },
                );
                SessionInfo {
                    id,
                    is_new: true,
                    visits: 1,
                }
            }
        }
    }

    /// The `Set-Cookie` header value for a session id.
    pub fn set_cookie_header(id: &str) -> String {
        format!("{SESSION_COOKIE}={id}; Path=/; HttpOnly; SameSite=Lax")
    }

    fn evict_expired(&mut self, now: Instant) {
        self.sessions
            .retain(|_, s| now.duration_since(s.last_seen) < SESSION_TTL);
    }
}

/// Find a single cookie's value within a `Cookie` header.
fn cookie_value(header: &str, name: &str) -> Option<String> {
    for pair in header.split(';') {
        let pair = pair.trim();
        if let Some((k, v)) = pair.split_once('=') {
            if k.trim() == name {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// 128 bits of randomness from `/dev/urandom`, hex-encoded. Falls back to a
/// time-derived value if the device is somehow unreadable.
fn random_id() -> String {
    let mut buf = [0u8; 16];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !ok {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        buf[..16].copy_from_slice(&nanos.to_le_bytes());
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cookie_value() {
        let h = "theme=dark; session_id=abc123; lang=en";
        assert_eq!(cookie_value(h, "session_id"), Some("abc123".to_string()));
        assert_eq!(cookie_value(h, "theme"), Some("dark".to_string()));
        assert_eq!(cookie_value(h, "missing"), None);
    }

    #[test]
    fn random_ids_are_unique_and_long() {
        let a = random_id();
        let b = random_id();
        assert_eq!(a.len(), 32); // 16 bytes * 2 hex chars
        assert_ne!(a, b);
    }

    #[test]
    fn new_then_returning_session() {
        let mut store = SessionStore::new();
        let first = store.touch(None);
        assert!(first.is_new);
        assert_eq!(first.visits, 1);

        let cookie = format!("session_id={}", first.id);
        let second = store.touch(Some(&cookie));
        assert!(!second.is_new);
        assert_eq!(second.visits, 2);
        assert_eq!(second.id, first.id);
    }
}
