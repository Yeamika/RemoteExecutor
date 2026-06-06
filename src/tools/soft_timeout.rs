use anyhow::{anyhow, Result};
use std::time::{Duration, Instant};

pub(crate) const DEFAULT_SEARCH_TIMEOUT_MS: i64 = 10_000;

#[derive(Debug)]
pub(crate) struct SoftTimeout {
    deadline: Option<Instant>,
    timed_out: bool,
}

impl SoftTimeout {
    pub(crate) fn from_millis(timeout: Option<i64>) -> Result<Self> {
        match timeout.unwrap_or(DEFAULT_SEARCH_TIMEOUT_MS) {
            -1 => Ok(Self {
                deadline: None,
                timed_out: false,
            }),
            timeout if timeout < -1 => Err(anyhow!("timeout must be -1 or non-negative")),
            timeout => Ok(Self {
                deadline: Some(Instant::now() + Duration::from_millis(timeout as u64)),
                timed_out: false,
            }),
        }
    }

    pub(crate) fn expired(&mut self) -> bool {
        if let Some(deadline) = self.deadline {
            if Instant::now() >= deadline {
                self.timed_out = true;
                return true;
            }
        }
        false
    }

    pub(crate) fn timed_out(&self) -> bool {
        self.timed_out
    }
}
