use dashmap::DashMap;
use std::collections::VecDeque;
use std::net::IpAddr;
use std::time::Instant;

use crate::error::AppError;

pub struct RateLimiter {
    requests: DashMap<IpAddr, VecDeque<Instant>>,
    max_requests: usize,
    window_secs: u64,
}

impl RateLimiter {
    pub fn new(max_requests: usize, window_secs: u64) -> Self {
        Self {
            requests: DashMap::new(),
            max_requests,
            window_secs,
        }
    }

    pub fn check(&self, ip: IpAddr) -> Result<(), AppError> {
        let now = Instant::now();
        let window = std::time::Duration::from_secs(self.window_secs);

        let mut entry = self.requests.entry(ip).or_insert_with(VecDeque::new);
        let timestamps = entry.value_mut();

        // Remove old entries
        while let Some(front) = timestamps.front() {
            if now.duration_since(*front) > window {
                timestamps.pop_front();
            } else {
                break;
            }
        }

        if timestamps.len() >= self.max_requests {
            return Err(AppError::TooManyRequests(
                "Too many requests, please try again later.".to_string(),
            ));
        }

        timestamps.push_back(now);
        Ok(())
    }
}
