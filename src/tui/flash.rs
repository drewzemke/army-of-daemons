use std::time::{Duration, Instant};

const SUCCESS_TTL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Error,
}

/// transient bottom-bar message. errors stick until the next keypress,
/// info clears on its own after SUCCESS_TTL
#[derive(Debug, Clone)]
pub struct Flash {
    pub msg: String,
    pub level: Level,
    pub at: Instant,
}

impl Flash {
    pub fn info(msg: impl Into<String>) -> Self {
        Self { msg: msg.into(), level: Level::Info, at: Instant::now() }
    }

    pub fn error(msg: impl Into<String>) -> Self {
        Self { msg: msg.into(), level: Level::Error, at: Instant::now() }
    }

    pub fn expired(&self, now: Instant) -> bool {
        self.level == Level::Info && now.duration_since(self.at) > SUCCESS_TTL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_expires_after_ttl() {
        let f = Flash::info("copied");
        assert_eq!(f.level, Level::Info);
        assert!(!f.expired(f.at + Duration::from_millis(500)));
        assert!(f.expired(f.at + SUCCESS_TTL + Duration::from_millis(1)));
    }

    #[test]
    fn errors_never_expire_on_time() {
        let f = Flash::error("on_read failed: nope");
        assert_eq!(f.level, Level::Error);
        assert!(!f.expired(f.at + Duration::from_secs(3600)));
    }
}
