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

"""Offline reference-data download; credentials stay in memory, never in outputs."""

import argparse
import ast
import gzip
import hashlib
import json
import re
import shutil
import time
import urllib.error
import urllib.parse
import urllib.request
from datetime import date
from pathlib import Path


def download(
    notebook,
    output,
    symbol="SPY",
    request_interval=0.6,
    start="2016-01-04",
    end="2025-12-31",
):
    if not re.fullmatch(r"[A-Z][A-Z0-9.-]{0,14}", symbol):
        raise ValueError("Invalid US stock symbol")
    if request_interval < 0.6:
        raise ValueError("Request interval must be at least 0.6 seconds")
    if date.fromisoformat(start) >= date.fromisoformat(end):
        raise ValueError("Start date must precede exclusive end date")
    metadata = {
        "provider": "Alpaca",
        "symbol": symbol,
        "timeframe": "1Min",
        "start": f"{start}T00:00:00Z",
        "end": f"{end}T00:00:00Z",
        "adjustment": "raw",
        "feed": "API default, same unspecified feed as Notebook",
    }
    output.mkdir(parents=True, exist_ok=True)
    manifest_path = output / "manifest.json"
    if manifest_path.exists():
        cached = json.loads(manifest_path.read_text())
        if any(cached.get(key) != value for key, value in metadata.items()):
            raise ValueError("Cache request mismatch; use a separate output directory")
    elif any(output.glob("page-*.json.gz")):
        raise ValueError("Unidentified cache pages: manifest missing")
    else:
        manifest_path.write_text(
            json.dumps({**metadata, "complete": False}, indent=2) + "\n"
        )
    cells = json.loads(notebook.read_text())["cells"]
    credentials = {}
    for node in ast.parse("".join(cells[3]["source"])).body:
        if isinstance(node, ast.Assign) and isinstance(node.targets[0], ast.Name):
            name = node.targets[0].id
            if name in ("API_KEY", "SECRET_KEY"):
                credentials[name] = ast.literal_eval(node.value)
    headers = {
        "APCA-API-KEY-ID": credentials["API_KEY"],
        "APCA-API-SECRET-KEY": credentials["SECRET_KEY"],
        "Accept-Encoding": "gzip",
    }

    def fetch(request):
        for attempt in range(5):
            try:
                with urllib.request.urlopen(request, timeout=45) as response:
                    body = (
                        gzip.GzipFile(fileobj=response)
                        if response.headers.get("Content-Encoding") == "gzip"
                        else response
                    )
                    payload = json.load(body)
                return payload
            except urllib.error.HTTPError as e:
                if e.code not in (429, 500, 502, 503, 504) or attempt == 4:
                    raise RuntimeError(
                        f"Alpaca history request failed: HTTP {e.code}"
                    ) from None
                time.sleep(
                    min(
                        30,
                        max(1, int(e.headers.get("Retry-After", "1"))) * (attempt + 1),
                    )
                )
            except (TimeoutError, urllib.error.URLError):
                if attempt == 4:
                    raise RuntimeError(
                        "Alpaca history request timed out; restart to resume cached pages"
                    ) from None
                time.sleep(2**attempt)

    token = None
    total = 0
    index = 0
    while True:
        path = output / f"page-{index:04d}.json.gz"
        if path.exists():
            with gzip.open(path, "rt") as stream:
                payload = json.load(stream)
        else:
            if shutil.disk_usage(output).free < 2 * 1024**3:
                raise RuntimeError(
                    "Download paused: disk free below 2 GiB; completed pages retained"
                )
            params = {
                "timeframe": "1Min",
                "start": metadata["start"],
                "end": metadata["end"],
                "limit": 10000,
                "adjustment": "raw",
                "sort": "asc",
            }
            if token:
                params["page_token"] = token
            request = urllib.request.Request(
                f"https://data.alpaca.markets/v2/stocks/{symbol}/bars?"
                + urllib.parse.urlencode(params),
                headers=headers,
            )
            payload = fetch(request)
            if payload.get("symbol") != symbol or "bars" not in payload:
                raise RuntimeError("Unexpected historical response")
            temporary = path.with_suffix(".tmp")
            with gzip.open(temporary, "wt") as stream:
                json.dump(payload, stream, separators=(",", ":"))
            temporary.replace(path)
            time.sleep(request_interval)
        if payload.get("symbol") != symbol or "bars" not in payload:
            raise RuntimeError(
                "Cached page symbol mismatch or invalid historical response"
            )
        bars = payload["bars"] or []
        total += len(bars)
        token = payload.get("next_page_token")
        if index % 10 == 0 or not token:
            print(
                f"symbol={symbol} page={index} bars={total} through={bars[-1]['t'] if bars else 'empty'}",
                flush=True,
            )
        index += 1
        if not token:
            break
    calendar_path = output / "calendar.json"
    if not calendar_path.exists():
        request = urllib.request.Request(
            f"https://paper-api.alpaca.markets/v2/calendar?start={start}&end={end}",
            headers=headers,
        )
        calendar = fetch(request)
        if not isinstance(calendar, list) or not calendar:
            raise RuntimeError("Invalid Alpaca calendar response")
        temporary = calendar_path.with_suffix(".tmp")
        temporary.write_text(json.dumps(calendar, separators=(",", ":")))
        temporary.replace(calendar_path)
    metadata.update(
        {
            "rows": total,
            "pages": index,
            "notebook_sha256": hashlib.sha256(notebook.read_bytes()).hexdigest(),
            "complete": True,
        }
    )
    temporary = manifest_path.with_suffix(".tmp")
    temporary.write_text(json.dumps(metadata, indent=2) + "\n")
    temporary.replace(manifest_path)
    print(json.dumps(metadata, indent=2), flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--notebook", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--symbol", default="SPY")
    parser.add_argument("--request-interval", type=float, default=0.6)
    parser.add_argument("--start", default="2016-01-04", help="UTC start date")
    parser.add_argument("--end", default="2025-12-31", help="Exclusive UTC end date")
    args = parser.parse_args()
    download(
        args.notebook,
        args.output,
        args.symbol,
        args.request_interval,
        args.start,
        args.end,
    )
