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

use std::sync::LazyLock;

use nautilus_model::identifiers::{ClientId, Venue};
use ustr::Ustr;

/// Longbridge venue identifier string.
pub const LONGBRIDGE: &str = "LONGBRIDGE";

/// Static Longbridge venue instance.
pub static LONGBRIDGE_VENUE: LazyLock<Venue> = LazyLock::new(|| Venue::new(Ustr::from(LONGBRIDGE)));

/// Static Longbridge client ID instance.
pub static LONGBRIDGE_CLIENT_ID: LazyLock<ClientId> =
    LazyLock::new(|| ClientId::new(Ustr::from(LONGBRIDGE)));

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    fn test_longbridge_constants() {
        assert_eq!(LONGBRIDGE_VENUE.as_str(), LONGBRIDGE);
        assert_eq!(LONGBRIDGE_CLIENT_ID.as_str(), LONGBRIDGE);
    }
}
