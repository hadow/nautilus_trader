// -------------------------------------------------------------------------------------------------
//  Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
//  https://nautechsystems.io
//
//  Licensed under the GNU Lesser General Public License Version 3.0 (the "License");
//  you may not use this file except in compliance with the License.
//  You may obtain a copy of the License at https://www.gnu.org/licenses/lgpl-3.0.en.html
// -------------------------------------------------------------------------------------------------

//! 独立运行 Longbridge 盘前筛选，并生成 SLC 交易节点可审计的当日动态标的池。

#[path = "slc/mod.rs"]
mod slc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    slc::run(false, true).await
}
