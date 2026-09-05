// -------------------------------------------------------------------------------------------------
//  Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
//  https://nautechsystems.io
//
//  Licensed under the GNU Lesser General Public License Version 3.0 (the "License");
//  You may not use this file except in compliance with the License.
//  You may obtain a copy of the License at
//  https://www.gnu.org/licenses/lgpl-3.0.en.html
//
//  Unless required by applicable law or agreed to in writing, software
//  distributed under the License is distributed on an "AS IS" BASIS,
//  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//  See the License for the specific language governing permissions and
//  limitations under the License.
// -------------------------------------------------------------------------------------------------

//! Longbridge API rate and connection guards.

use std::{
    collections::VecDeque,
    future::Future,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const QUOTE_MAX_CALLS: usize = 10;
// Leave room for network arrival jitter above Longbridge's strict one-second boundary
const QUOTE_WINDOW: Duration = Duration::from_millis(1_100);
const QUOTE_MAX_CONCURRENCY: usize = 5;
const QUOTE_RATE_LIMIT_ERROR_CODE: i64 = 301606;
const QUOTE_RATE_LIMIT_MAX_RETRIES: usize = 3;
const TRADE_MAX_CALLS: usize = 30;
const TRADE_WINDOW: Duration = Duration::from_secs(30);
// Leave room for network and server clock granularity above Longbridge's strict 20ms boundary.
const TRADE_MIN_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Debug)]
struct RateState {
    calls: VecDeque<Instant>,
    last_call: Option<Instant>,
}

/// Guard held for the duration of one rate-limited API call.
#[derive(Debug)]
struct RateLimitPermit {
    _concurrency: Option<OwnedSemaphorePermit>,
}

#[derive(Debug)]
struct RateLimiter {
    max_calls: usize,
    window: Duration,
    min_interval: Duration,
    concurrency: Option<Arc<Semaphore>>,
    state: tokio::sync::Mutex<RateState>,
}

impl RateLimiter {
    fn new(
        max_calls: usize,
        window: Duration,
        min_interval: Duration,
        max_concurrency: Option<usize>,
    ) -> Self {
        Self {
            max_calls,
            window,
            min_interval,
            concurrency: max_concurrency.map(|value| Arc::new(Semaphore::new(value))),
            state: tokio::sync::Mutex::new(RateState {
                calls: VecDeque::with_capacity(max_calls),
                last_call: None,
            }),
        }
    }

    /// Waits until the next API call is legal under the configured rate rules.
    async fn acquire(&self) -> RateLimitPermit {
        let concurrency = match &self.concurrency {
            Some(semaphore) => Some(
                semaphore
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("rate limiter semaphore closed"),
            ),
            None => None,
        };

        loop {
            let now = Instant::now();
            let wait = {
                let mut state = self.state.lock().await;
                while state
                    .calls
                    .front()
                    .is_some_and(|timestamp| now.duration_since(*timestamp) >= self.window)
                {
                    state.calls.pop_front();
                }

                let window_wait = if state.calls.len() >= self.max_calls {
                    state.calls.front().map_or(Duration::ZERO, |timestamp| {
                        (*timestamp + self.window).saturating_duration_since(now)
                    })
                } else {
                    Duration::ZERO
                };

                let interval_wait = state.last_call.map_or(Duration::ZERO, |timestamp| {
                    (timestamp + self.min_interval).saturating_duration_since(now)
                });

                window_wait.max(interval_wait)
            };

            if wait.is_zero() {
                let now = Instant::now();
                let mut state = self.state.lock().await;
                while state
                    .calls
                    .front()
                    .is_some_and(|timestamp| now.duration_since(*timestamp) >= self.window)
                {
                    state.calls.pop_front();
                }

                let window_ready = state.calls.len() < self.max_calls;
                let interval_ready = state
                    .last_call
                    .is_none_or(|timestamp| now.duration_since(timestamp) >= self.min_interval);

                if window_ready && interval_ready {
                    state.calls.push_back(now);
                    state.last_call = Some(now);
                    break;
                }
            } else {
                tokio::time::sleep(wait).await;
            }
        }

        RateLimitPermit {
            _concurrency: concurrency,
        }
    }
}

/// Longbridge quote APIs: at most 10 calls per second and 5 concurrent requests.
fn quote_rate_limiter() -> &'static RateLimiter {
    static LIMITER: LazyLock<RateLimiter> = LazyLock::new(|| {
        RateLimiter::new(
            QUOTE_MAX_CALLS,
            QUOTE_WINDOW,
            Duration::ZERO,
            Some(QUOTE_MAX_CONCURRENCY),
        )
    });
    &LIMITER
}

/// Longbridge trade APIs: at most 30 calls per 30 seconds and at least 20ms between calls.
fn trade_rate_limiter() -> &'static RateLimiter {
    static LIMITER: LazyLock<RateLimiter> =
        LazyLock::new(|| RateLimiter::new(TRADE_MAX_CALLS, TRADE_WINDOW, TRADE_MIN_INTERVAL, None));
    &LIMITER
}

/// Executes one quote API call after acquiring the process-wide quote limits.
#[doc(hidden)]
pub async fn quote_api_call<F>(call: F) -> F::Output
where
    F: Future,
{
    let _permit = quote_rate_limiter().acquire().await;
    call.await
}

/// Executes a repeatable quote API call with bounded rate-limit retries.
#[doc(hidden)]
#[allow(clippy::result_large_err)] // Preserve the SDK error type for existing adapter callers
pub async fn quote_api_call_with_retry<F, Fut, T>(mut call: F) -> longbridge::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = longbridge::Result<T>>,
{
    let mut retries = 0;
    loop {
        match quote_api_call(call()).await {
            Err(e)
                if e.openapi_error_code() == Some(QUOTE_RATE_LIMIT_ERROR_CODE)
                    && retries < QUOTE_RATE_LIMIT_MAX_RETRIES =>
            {
                retries += 1;
                log::warn!(
                    "Longbridge quote API rate limited; retrying in {} ms ({retries}/{QUOTE_RATE_LIMIT_MAX_RETRIES})",
                    QUOTE_WINDOW.as_millis(),
                );
                tokio::time::sleep(QUOTE_WINDOW).await;
            }
            result => return result,
        }
    }
}

/// Executes one trade API call after acquiring the process-wide trade limits.
pub(crate) async fn trade_api_call<F>(call: F) -> F::Output
where
    F: Future,
{
    let _permit = trade_rate_limiter().acquire().await;
    call.await
}

static QUOTE_CONNECTION_HELD: AtomicBool = AtomicBool::new(false);

/// Process-local guard for Longbridge's single quote WebSocket connection rule.
///
/// This prevents accidental duplicate quote connections inside one process. A separate process
/// cannot be coordinated by this in-memory guard and must still be prevented operationally.
#[derive(Debug)]
pub(crate) struct QuoteConnectionGuard {
    _private: (),
}

impl Drop for QuoteConnectionGuard {
    fn drop(&mut self) {
        QUOTE_CONNECTION_HELD.store(false, Ordering::Release);
    }
}

/// Attempts to acquire the process-local quote WebSocket slot.
pub(crate) fn try_acquire_quote_connection() -> Option<QuoteConnectionGuard> {
    QUOTE_CONNECTION_HELD
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
        .then_some(QuoteConnectionGuard { _private: () })
}

/// Maximum number of unique symbols simultaneously subscribed on a quote connection.
#[doc(hidden)]
pub const MAX_QUOTE_SUBSCRIPTION_SYMBOLS: usize = 500;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_trade_rate_limiter_enforces_minimum_interval() {
        let limiter = RateLimiter::new(TRADE_MAX_CALLS, TRADE_WINDOW, TRADE_MIN_INTERVAL, None);
        let started = Instant::now();
        drop(limiter.acquire().await);
        drop(limiter.acquire().await);

        assert!(started.elapsed() >= TRADE_MIN_INTERVAL);
    }

    #[test]
    fn test_trade_rate_limiter_keeps_margin_above_server_boundary() {
        assert!(TRADE_MIN_INTERVAL > Duration::from_millis(20));
    }

    #[test]
    fn test_quote_rate_limiter_keeps_margin_above_server_boundary() {
        assert!(QUOTE_WINDOW > Duration::from_secs(1));
    }

    #[tokio::test]
    async fn test_quote_rate_limiter_caps_concurrency() {
        let limiter = RateLimiter::new(10, Duration::from_secs(1), Duration::ZERO, Some(2));
        let first = limiter.acquire().await;
        let second = limiter.acquire().await;

        assert!(
            tokio::time::timeout(Duration::from_millis(10), limiter.acquire())
                .await
                .is_err()
        );
        drop(first);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), limiter.acquire())
                .await
                .is_ok()
        );
        drop(second);
    }

    #[tokio::test]
    async fn test_quote_api_call_retries_rate_limit_response() {
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let result = quote_api_call_with_retry(|| async {
            if attempts.fetch_add(1, Ordering::Relaxed) == 0 {
                Err(longbridge::Error::WsClient(
                    longbridge::wsclient::WsClientError::ResponseError {
                        status: 3,
                        detail: Some(longbridge::wsclient::WsResponseErrorDetail {
                            code: 301606,
                            msg: "request rate limit".to_string(),
                        }),
                    },
                ))
            } else {
                Ok(())
            }
        })
        .await;

        assert!(result.is_ok());
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn test_rate_limiter_enforces_rolling_window() {
        let window = Duration::from_millis(20);
        let limiter = RateLimiter::new(2, window, Duration::ZERO, None);
        let started = Instant::now();
        drop(limiter.acquire().await);
        drop(limiter.acquire().await);
        drop(limiter.acquire().await);

        assert!(started.elapsed() >= window);
    }

    #[test]
    fn test_quote_connection_guard_is_exclusive() {
        let first = try_acquire_quote_connection().expect("first quote connection");
        assert!(try_acquire_quote_connection().is_none());
        drop(first);
        assert!(try_acquire_quote_connection().is_some());
    }
}
