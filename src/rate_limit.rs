use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Mutex,
    time::{Duration, Instant},
};

use axum::http::HeaderMap;

use crate::{AppError, AppResult};

/// Fixed-window counter for create requests. One window is shared globally
/// and one window is kept per caller key.
pub struct RateLimiter {
    per_key: Mutex<HashMap<String, Window>>,
    global: Mutex<Window>,
    per_key_limit: u32,
    global_limit: u32,
    window: Duration,
}

struct Window {
    start: Instant,
    count: u32,
}

impl RateLimiter {
    #[must_use]
    pub fn new(per_key_limit: u32, global_limit: u32, window: Duration) -> Self {
        let now = Instant::now();
        Self {
            per_key: Mutex::new(HashMap::new()),
            global: Mutex::new(Window {
                start: now,
                count: 0,
            }),
            per_key_limit,
            global_limit,
            window,
        }
    }

    /// Records one create attempt. Returns an error when the caller or the
    /// process has used its quota for the current window.
    pub fn check(&self, key: &str) -> AppResult<()> {
        if self.per_key_limit == 0 || self.global_limit == 0 {
            return Ok(());
        }
        let now = Instant::now();
        {
            let mut global = self
                .global
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !allow(&mut global, now, self.window, self.global_limit) {
                return Err(AppError::RateLimited);
            }
        }
        let mut per_key = self
            .per_key
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if per_key.len() > 10_000 {
            per_key.retain(|_, window| now.duration_since(window.start) < self.window);
        }
        let window = per_key.entry(key.to_owned()).or_insert(Window {
            start: now,
            count: 0,
        });
        if allow(window, now, self.window, self.per_key_limit) {
            Ok(())
        } else {
            Err(AppError::RateLimited)
        }
    }
}

fn allow(window: &mut Window, now: Instant, length: Duration, limit: u32) -> bool {
    if now.duration_since(window.start) >= length {
        window.start = now;
        window.count = 0;
    }
    if window.count >= limit {
        return false;
    }
    window.count = window.count.saturating_add(1);
    true
}

/// Prefers the first `X-Forwarded-For` hop, then `X-Real-IP`, then the
/// connected peer. The reverse proxy must overwrite these headers.
#[must_use]
pub fn caller_key(headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    if let Some(forwarded) = header_text(headers, "x-forwarded-for")
        && let Some(first) = forwarded.split(',').next()
    {
        let first = first.trim();
        if !first.is_empty() {
            return format!("ip:{first}");
        }
    }
    if let Some(real_ip) = header_text(headers, "x-real-ip") {
        let real_ip = real_ip.trim();
        if !real_ip.is_empty() {
            return format!("ip:{real_ip}");
        }
    }
    peer.map_or_else(
        || "ip:unknown".to_owned(),
        |addr| format!("ip:{}", addr.ip()),
    )
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn rejects_the_call_after_the_per_key_limit() {
        let limiter = RateLimiter::new(2, 10, Duration::from_mins(1));
        assert!(limiter.check("ip:1").is_ok());
        assert!(limiter.check("ip:1").is_ok());
        assert!(matches!(limiter.check("ip:1"), Err(AppError::RateLimited)));
        assert!(limiter.check("ip:2").is_ok());
    }

    #[test]
    fn rejects_the_call_after_the_global_limit() {
        let limiter = RateLimiter::new(10, 2, Duration::from_mins(1));
        assert!(limiter.check("ip:1").is_ok());
        assert!(limiter.check("ip:2").is_ok());
        assert!(matches!(limiter.check("ip:3"), Err(AppError::RateLimited)));
    }

    #[test]
    fn treats_a_zero_limit_as_disabled() {
        let limiter = RateLimiter::new(0, 1, Duration::from_mins(1));
        assert!(limiter.check("ip:1").is_ok());
        assert!(limiter.check("ip:1").is_ok());
    }

    #[test]
    fn prefers_the_forwarded_client() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.8, 10.0.0.1"),
        );
        let peer = "127.0.0.1:9000".parse().unwrap();
        assert_eq!(caller_key(&headers, Some(peer)), "ip:203.0.113.8");
    }
}
