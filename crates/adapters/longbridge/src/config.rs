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

use std::{
    collections::HashMap,
    fmt::Debug,
    sync::{Arc, LazyLock},
};

use anyhow::Context;
use longbridge::{
    Config,
    oauth::{OAuth, OAuthBuilder},
};
use nautilus_model::enums::AccountType;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// Default local callback port used by the Longbridge OAuth 2.0 flow.
pub const DEFAULT_OAUTH_CALLBACK_PORT: u16 = 60_355;

#[derive(Clone)]
struct CachedOAuth {
    callback_port: u16,
    oauth: OAuth,
}

static OAUTH_CLIENTS: LazyLock<Mutex<HashMap<String, CachedOAuth>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn env_value(name: &str) -> Option<String> {
    std::env::var(format!("LONGBRIDGE_{name}"))
        .ok()
        .or_else(|| std::env::var(format!("LONGPORT_{name}")).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn resolve_oauth_client_id(oauth_client_id: Option<&str>) -> anyhow::Result<String> {
    oauth_client_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| env_value("OAUTH_CLIENT_ID"))
        .context(concat!(
            "Longbridge OAuth client ID not available; set LONGBRIDGE_OAUTH_CLIENT_ID ",
            "or pass oauth_client_id",
        ))
}

fn validate_callback_port(callback_port: u16) -> anyhow::Result<()> {
    if callback_port == 0 {
        anyhow::bail!("Longbridge OAuth callback port must be greater than zero");
    }
    Ok(())
}

async fn oauth_client(oauth_client_id: Option<&str>, callback_port: u16) -> anyhow::Result<OAuth> {
    let client_id = resolve_oauth_client_id(oauth_client_id)?;
    validate_callback_port(callback_port)?;

    let mut clients = OAUTH_CLIENTS.lock().await;
    if let Some(cached) = clients.get(&client_id) {
        if cached.callback_port != callback_port {
            anyhow::bail!(
                concat!(
                    "Longbridge OAuth client {} is already configured with callback port {}, ",
                    "cannot also use {}",
                ),
                client_id,
                cached.callback_port,
                callback_port,
            );
        }
        return Ok(cached.oauth.clone());
    }

    let oauth = OAuthBuilder::new(&client_id)
        .callback_port(callback_port)
        .build(|url| {
            log::info!("Open this URL to authorize Longbridge OAuth 2.0: {url}");
        })
        .await
        .context("failed to initialize Longbridge OAuth 2.0 authorization")?;
    clients.insert(
        client_id,
        CachedOAuth {
            callback_port,
            oauth: oauth.clone(),
        },
    );
    Ok(oauth)
}

async fn sdk_config(
    oauth_client_id: Option<&str>,
    oauth_callback_port: u16,
    http_url: Option<&str>,
    quote_ws_url: Option<&str>,
    trade_ws_url: Option<&str>,
    enable_overnight: bool,
    papertrading: bool,
) -> anyhow::Result<Arc<Config>> {
    let oauth = oauth_client(oauth_client_id, oauth_callback_port).await?;
    let mut config = Config::from_oauth(oauth);

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
    /// OAuth 2.0 public client ID (falls back to `LONGBRIDGE_OAUTH_CLIENT_ID`).
    pub oauth_client_id: Option<String>,
    /// Local port for the OAuth 2.0 redirect callback.
    #[builder(default = DEFAULT_OAUTH_CALLBACK_PORT)]
    pub oauth_callback_port: u16,
    /// Optional HTTP endpoint override.
    pub http_url: Option<String>,
    /// Optional quote WebSocket endpoint override.
    pub quote_ws_url: Option<String>,
    /// Whether to request US overnight quote data.
    #[builder(default)]
    pub enable_overnight: bool,
}

impl Debug for LongbridgeDataClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(stringify!(LongbridgeDataClientConfig))
            .field("oauth_client_id", &self.oauth_client_id)
            .field("oauth_callback_port", &self.oauth_callback_port)
            .field("http_url", &self.http_url)
            .field("quote_ws_url", &self.quote_ws_url)
            .field("enable_overnight", &self.enable_overnight)
            .finish()
    }
}

#[cfg(feature = "python")]
nautilus_core::impl_pyo3_config_getters!(LongbridgeDataClientConfig {
    oauth_client_id: Option<String>,
    oauth_callback_port: u16,
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
    /// Builds an OAuth-backed SDK configuration.
    ///
    /// The official SDK loads and refreshes the token in its default storage. If no usable token
    /// exists, this waits for the browser authorization callback.
    ///
    /// # Errors
    ///
    /// Returns an error if the OAuth settings are invalid or authorization fails.
    pub async fn sdk_config(&self) -> anyhow::Result<Arc<Config>> {
        sdk_config(
            self.oauth_client_id.as_deref(),
            self.oauth_callback_port,
            self.http_url.as_deref(),
            self.quote_ws_url.as_deref(),
            None,
            self.enable_overnight,
            false,
        )
        .await
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
    /// OAuth 2.0 public client ID (falls back to `LONGBRIDGE_OAUTH_CLIENT_ID`).
    pub oauth_client_id: Option<String>,
    /// Local port for the OAuth 2.0 redirect callback.
    #[builder(default = DEFAULT_OAUTH_CALLBACK_PORT)]
    pub oauth_callback_port: u16,
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

impl Debug for LongbridgeExecClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(stringify!(LongbridgeExecClientConfig))
            .field("oauth_client_id", &self.oauth_client_id)
            .field("oauth_callback_port", &self.oauth_callback_port)
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
    oauth_client_id: Option<String>,
    oauth_callback_port: u16,
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
    /// Builds an OAuth-backed SDK configuration.
    ///
    /// The official SDK loads and refreshes the token in its default storage. If no usable token
    /// exists, this waits for the browser authorization callback.
    ///
    /// # Errors
    ///
    /// Returns an error if the OAuth settings are invalid or authorization fails.
    pub async fn sdk_config(&self) -> anyhow::Result<Arc<Config>> {
        sdk_config(
            self.oauth_client_id.as_deref(),
            self.oauth_callback_port,
            self.http_url.as_deref(),
            None,
            self.trade_ws_url.as_deref(),
            false,
            self.papertrading,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    fn test_defaults_use_official_oauth_callback_port() {
        let data_config = LongbridgeDataClientConfig::default();
        assert!(data_config.oauth_client_id.is_none());
        assert_eq!(data_config.oauth_callback_port, DEFAULT_OAUTH_CALLBACK_PORT);
        assert!(!data_config.enable_overnight);

        let exec_config = LongbridgeExecClientConfig::default();
        assert!(exec_config.oauth_client_id.is_none());
        assert_eq!(exec_config.oauth_callback_port, DEFAULT_OAUTH_CALLBACK_PORT);
        assert_eq!(exec_config.account_type, AccountType::Margin);
        assert!(!exec_config.papertrading);
        assert!(!exec_config.outside_rth);
    }

    #[rstest]
    fn test_explicit_oauth_client_id_is_trimmed() {
        assert_eq!(
            resolve_oauth_client_id(Some("  public-client-id  ")).unwrap(),
            "public-client-id",
        );
    }

    #[rstest]
    fn test_zero_callback_port_is_rejected() {
        let error = validate_callback_port(0).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Longbridge OAuth callback port must be greater than zero",
        );
    }

    #[rstest]
    fn test_debug_includes_public_oauth_settings() {
        let config = LongbridgeExecClientConfig {
            oauth_client_id: Some("public-client-id".to_string()),
            oauth_callback_port: 60_400,
            ..Default::default()
        };
        let debug = format!("{config:?}");
        assert!(debug.contains("public-client-id"));
        assert!(debug.contains("60400"));
    }
}
