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
//  See the License for the specific language governing permissions and limitations under the License.
// -------------------------------------------------------------------------------------------------

use std::{any::Any, sync::Arc};

use nautilus_core::UnixNanos;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::{Bar, CustomDataTrait, HasTsInit};

/// A completed bar with its provider volume-weighted price, preserved without OHLC approximation.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct BarWithVwap {
    pub bar: Bar,
    pub vwap: Decimal,
}

impl HasTsInit for BarWithVwap {
    fn ts_init(&self) -> UnixNanos {
        self.bar.ts_init
    }
}

impl CustomDataTrait for BarWithVwap {
    fn type_name(&self) -> &'static str {
        Self::type_name_static()
    }
    fn type_name_static() -> &'static str {
        "BarWithVwap"
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn ts_event(&self) -> UnixNanos {
        self.bar.ts_event
    }
    fn to_json(&self) -> anyhow::Result<String> {
        Ok(serde_json::to_string(self)?)
    }
    fn from_json(value: serde_json::Value) -> anyhow::Result<Arc<dyn CustomDataTrait>> {
        Ok(Arc::new(serde_json::from_value::<Self>(value)?))
    }
    fn clone_arc(&self) -> Arc<dyn CustomDataTrait> {
        Arc::new(*self)
    }
    fn eq_arc(&self, other: &dyn CustomDataTrait) -> bool {
        other.as_any().downcast_ref::<Self>() == Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn provider_vwap_roundtrip_preserves_exact_value_and_times() {
        let bar = Bar::new(
            "SPY.SIM-1-MINUTE-LAST-EXTERNAL".parse().unwrap(),
            "100.00".into(),
            "100.01".into(),
            "99.99".into(),
            "100.00".into(),
            100.into(),
            60_000_000_000.into(),
            60_000_000_001.into(),
        );
        let value = BarWithVwap {
            bar,
            vwap: Decimal::new(100000123, 6),
        };
        let decoded =
            BarWithVwap::from_json(serde_json::from_str(&value.to_json().unwrap()).unwrap())
                .unwrap();
        assert!(value.eq_arc(decoded.as_ref()));
        assert_eq!(decoded.ts_event(), bar.ts_event);
        assert_eq!(decoded.ts_init(), bar.ts_init);
    }
}
