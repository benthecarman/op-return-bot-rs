use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
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

/// Identifies the caller of a create request. Forwarding headers are only
/// trusted from a loopback peer, the local reverse proxy; any other peer is
/// keyed by its own address. From the proxy, the last `X-Forwarded-For` hop
/// is used because the proxy appended it. Earlier hops come from the client
/// and can be forged. `X-Real-IP` is used when there is no `X-Forwarded-For`.
#[must_use]
pub fn caller_key(headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    let Some(peer) = peer else {
        return "ip:unknown".to_owned();
    };
    if !is_loopback(peer.ip()) {
        return format!("ip:{}", peer.ip());
    }
    let forwarded = headers
        .get_all("x-forwarded-for")
        .iter()
        .next_back()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit(',').next())
        .or_else(|| header_text(headers, "x-real-ip"))
        .and_then(|value| value.trim().parse::<IpAddr>().ok());
    format!("ip:{}", forwarded.unwrap_or_else(|| peer.ip()))
}

#[must_use]
pub fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback())
        }
    }
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

    fn forwarded(values: &[&'static str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append("x-forwarded-for", HeaderValue::from_static(value));
        }
        headers
    }

    #[test]
    fn treats_only_loopback_as_local() {
        assert!(is_loopback("127.0.0.1".parse().unwrap()));
        assert!(is_loopback("::1".parse().unwrap()));
        assert!(is_loopback("::ffff:127.0.0.1".parse().unwrap()));
        assert!(!is_loopback("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn uses_the_hop_that_the_local_proxy_appended() {
        let peer = "127.0.0.1:9000".parse().unwrap();
        assert_eq!(
            caller_key(&forwarded(&["203.0.113.8"]), Some(peer)),
            "ip:203.0.113.8"
        );
        assert_eq!(
            caller_key(&forwarded(&["198.51.100.1, 203.0.113.8"]), Some(peer)),
            "ip:203.0.113.8"
        );
        assert_eq!(
            caller_key(&forwarded(&["198.51.100.1", "203.0.113.8"]), Some(peer)),
            "ip:203.0.113.8"
        );
    }

    #[test]
    fn ignores_forwarding_headers_from_remote_peers() {
        let peer = "192.0.2.4:50000".parse().unwrap();
        let mut headers = forwarded(&["203.0.113.8"]);
        headers.insert("x-real-ip", HeaderValue::from_static("203.0.113.9"));
        assert_eq!(caller_key(&headers, Some(peer)), "ip:192.0.2.4");
    }

    #[test]
    fn falls_back_to_the_peer_for_invalid_addresses() {
        let peer = "127.0.0.1:9000".parse().unwrap();
        assert_eq!(
            caller_key(&forwarded(&["203.0.113.8, not-an-ip"]), Some(peer)),
            "ip:127.0.0.1"
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", HeaderValue::from_static("203.0.113.9"));
        assert_eq!(caller_key(&headers, Some(peer)), "ip:203.0.113.9");
    }
}
