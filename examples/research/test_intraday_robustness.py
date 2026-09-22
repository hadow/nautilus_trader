# -------------------------------------------------------------------------------------------------
#  Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
#  https://nautechsystems.io
#
#  Licensed under the GNU Lesser General Public License Version 3.0 (the "License");
#  You may not use this file except in compliance with the License.
#  You may obtain a copy of the License at https://www.gnu.org/licenses/lgpl-3.0.en.html
#
#  Unless required by applicable law or agreed to in writing, software
#  distributed under the License is distributed on an "AS IS" BASIS,
#  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
#  See the License for the specific language governing permissions and
#  limitations under the License.
# -------------------------------------------------------------------------------------------------

"""Checks compounding and deterministic block sampling of native daily returns."""

import unittest

from intraday_robustness import bootstrap_mean, return_stats


class RobustnessStatisticsTest(unittest.TestCase):
    def test_compounding_drawdown_and_tail_use_observed_daily_returns(self):
        result = return_stats([0.1, -0.1])
        self.assertAlmostEqual(result["total_return"], -0.01)
        self.assertAlmostEqual(result["max_drawdown"], 0.1)
        self.assertEqual(result["expected_shortfall_95"], -0.1)
        self.assertEqual(result["sharpe"], 0)

    def test_block_bootstrap_is_deterministic_and_preserves_zero_difference(self):
        for block in [5, 10, 20]:
            result = bootstrap_mean([0.0] * 37, block, 200)
            self.assertEqual(result["ci95_low"], 0)
            self.assertEqual(result["ci95_high"], 0)
            self.assertEqual(result["p_two_sided"], 1)
            values = [0.01] * 15 + [-0.02] * 22
            self.assertEqual(
                bootstrap_mean(values, block, 200), bootstrap_mean(values, block, 200)
            )


if __name__ == "__main__":
    unittest.main()
