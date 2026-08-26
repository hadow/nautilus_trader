// -------------------------------------------------------------------------------------------------
//  Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
//  https://nautechsystems.io
//
//  Licensed under the GNU Lesser General Public License Version 3.0 (the "License");
//  You may not use this file except in compliance with the License.
//  You may obtain a copy of the License at https://www.gnu.org/licenses/lgpl-3.0.en.html
//
//  Unless required by applicable law or agreed to in writing, software
//  distributed under the License is distributed on an "AS IS" BASIS,
//  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//  See the License for the specific language governing permissions and
//  limitations under the License.
// -------------------------------------------------------------------------------------------------

//! Configuration for the Longbridge adapter.

use std::{fmt, sync::Arc};

use anyhow::Context;
use longbridge::Config;
use nautilus_model::enums::AccountType;
use serde::{Deserialize, Serialize};

fn env_credential(name: &str) -> Option<String> {
    std::env::var(format!("LONGBRIDGE_{name}"))
        .ok()
        .or_else(|| std::env::var(format!("LONGPORT_{name}")).ok())
        .filter(|value| !value.trim().is_empty())
}

fn sdk_config(
    app_key: Option<&str>,
    app_secret: Option<&str>,
    access_token: Option<&str>,
    http_url: Option<&str>,
    quote_ws_url: Option<&str>,
    trade_ws_url: Option<&str>,
    enable_overnight: bool,
    papertrading: bool,
) -> anyhow::Result<Arc<Config>> {
    let app_key = app_key
        .map(ToOwned::to_owned)
        .or_else(|| env_credential("APP_KEY"))
        .context("Longbridge app key not available; set LONGBRIDGE_APP_KEY or pass app_key")?;
    let app_secret = app_secret
        .map(ToOwned::to_owned)
        .or_else(|| env_credential("APP_SECRET"))
        .context(
            "Longbridge app secret not available; set LONGBRIDGE_APP_SECRET or pass app_secret",
        )?;
    let access_token = access_token
        .map(ToOwned::to_owned)
        .or_else(|| env_credential("ACCESS_TOKEN"))
        .context(
            "Longbridge access token not available; set LONGBRIDGE_ACCESS_TOKEN or pass access_token",
        )?;

    let mut config = Config::from_apikey(app_key, app_secret, access_token);
    if let Some(url) = http_url {
        config = config.http_url(url);
    }
    if let Some(url) = quote_ws_url {
        config = config.quote_ws_url(url);
    }
    if let Some(url) = trade_ws_url {
        config = config.trade_ws_url(url);
    }
    if enable_overnight {
        config = config.enable_overnight();
    }
    if papertrading {
        config = config.enable_papertrading();
    }

    Ok(Arc::new(config))
}

/// Configuration for the Longbridge live data client.
#[derive(Clone, Serialize, Deserialize, bon::Builder)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(
    feature = "python",
    pyo3::pyclass(module = "nautilus_trader.adapters.longbridge", from_py_object)
)]
#[cfg_attr(
    feature = "python",
    pyo3_stub_gen::derive::gen_stub_pyclass(module = "nautilus_trader.adapters.longbridge")
)]
pub struct LongbridgeDataClientConfig {
    /// Longbridge application key (falls back to `LONGBRIDGE_APP_KEY`).
    pub app_key: Option<String>,
    /// Longbridge application secret (falls back to `LONGBRIDGE_APP_SECRET`).
    pub app_secret: Option<String>,
    /// Longbridge access token (falls back to `LONGBRIDGE_ACCESS_TOKEN`).
    pub access_token: Option<String>,
    /// Optional HTTP endpoint override.
    pub http_url: Option<String>,
    /// Optional quote WebSocket endpoint override.
    pub quote_ws_url: Option<String>,
    /// Whether to request US overnight quote data.
    #[builder(default)]
    pub enable_overnight: bool,
}

impl fmt::Debug for LongbridgeDataClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct(stringify!(LongbridgeDataClientConfig))
            .field("app_key", &self.app_key.as_ref().map(|_| "<redacted>"))
            .field(
                "app_secret",
                &self.app_secret.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "<redacted>"),
            )
            .field("http_url", &self.http_url)
            .field("quote_ws_url", &self.quote_ws_url)
            .field("enable_overnight", &self.enable_overnight)
            .finish()
    }
}

#[cfg(feature = "python")]
nautilus_core::impl_pyo3_config_getters!(LongbridgeDataClientConfig {
    http_url: Option<String>,
    quote_ws_url: Option<String>,
    enable_overnight: bool,
});

impl Default for LongbridgeDataClientConfig {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl LongbridgeDataClientConfig {
    /// Builds an SDK configuration after resolving credential environment fallbacks.
    ///
    /// # Errors
    ///
    /// Returns an error if any credential is unavailable.
    pub fn sdk_config(&self) -> anyhow::Result<Arc<Config>> {
        sdk_config(
            self.app_key.as_deref(),
            self.app_secret.as_deref(),
            self.access_token.as_deref(),
            self.http_url.as_deref(),
            self.quote_ws_url.as_deref(),
            None,
            self.enable_overnight,
            false,
        )
    }
}

/// Configuration for the Longbridge live execution client.
#[derive(Clone, Serialize, Deserialize, bon::Builder)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(
    feature = "python",
    pyo3::pyclass(module = "nautilus_trader.adapters.longbridge", from_py_object)
)]
#[cfg_attr(
    feature = "python",
    pyo3_stub_gen::derive::gen_stub_pyclass(module = "nautilus_trader.adapters.longbridge")
)]
pub struct LongbridgeExecClientConfig {
    /// Longbridge application key (falls back to `LONGBRIDGE_APP_KEY`).
    pub app_key: Option<String>,
    /// Longbridge application secret (falls back to `LONGBRIDGE_APP_SECRET`).
    pub app_secret: Option<String>,
    /// Longbridge access token (falls back to `LONGBRIDGE_ACCESS_TOKEN`).
    pub access_token: Option<String>,
    /// Optional HTTP endpoint override.
    pub http_url: Option<String>,
    /// Optional trade WebSocket endpoint override.
    pub trade_ws_url: Option<String>,
    /// Account type reported to Nautilus (`CASH` or `MARGIN`).
    #[builder(default = AccountType::Margin)]
    pub account_type: AccountType,
    /// Whether to route execution requests to Longbridge paper trading.
    #[builder(default)]
    pub papertrading: bool,
    /// Whether submitted orders may execute outside regular trading hours.
    #[builder(default)]
    pub outside_rth: bool,
}

impl fmt::Debug for LongbridgeExecClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct(stringify!(LongbridgeExecClientConfig))
            .field("app_key", &self.app_key.as_ref().map(|_| "<redacted>"))
            .field(
                "app_secret",
                &self.app_secret.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "<redacted>"),
            )
            .field("http_url", &self.http_url)
            .field("trade_ws_url", &self.trade_ws_url)
            .field("account_type", &self.account_type)
            .field("papertrading", &self.papertrading)
            .field("outside_rth", &self.outside_rth)
            .finish()
    }
}

#[cfg(feature = "python")]
nautilus_core::impl_pyo3_config_getters!(LongbridgeExecClientConfig {
    http_url: Option<String>,
    trade_ws_url: Option<String>,
    account_type: AccountType,
    papertrading: bool,
    outside_rth: bool,
});

impl Default for LongbridgeExecClientConfig {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl LongbridgeExecClientConfig {
    /// Builds an SDK configuration after resolving credential environment fallbacks.
    ///
    /// # Errors
    ///
    /// Returns an error if any credential is unavailable.
    pub fn sdk_config(&self) -> anyhow::Result<Arc<Config>> {
        sdk_config(
            self.app_key.as_deref(),
            self.app_secret.as_deref(),
            self.access_token.as_deref(),
            self.http_url.as_deref(),
            None,
            self.trade_ws_url.as_deref(),
            false,
            self.papertrading,
        )
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    fn test_defaults() {
        assert!(!LongbridgeDataClientConfig::default().enable_overnight);
        let config = LongbridgeExecClientConfig::default();
        assert_eq!(config.account_type, AccountType::Margin);
        assert!(!config.papertrading);
        assert!(!config.outside_rth);
    }

    #[rstest]
    fn test_debug_redacts_credentials() {
        let config = LongbridgeExecClientConfig {
            app_key: Some("key-secret".to_string()),
            app_secret: Some("app-secret".to_string()),
            access_token: Some("token-secret".to_string()),
            ..Default::default()
        };
        let debug = format!("{config:?}");
        assert!(!debug.contains("key-secret"));
        assert!(!debug.contains("app-secret"));
        assert!(!debug.contains("token-secret"));
        assert!(debug.contains("<redacted>"));
    }
}
