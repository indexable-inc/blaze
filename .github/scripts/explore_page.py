"""explore_page.py: the deployed explorer at ix.dev/explore reads the profile schema this tree writes.

    uv run --frozen python .github/scripts/explore_page.py '{}'
    uv run --frozen python .github/scripts/explore_page.py '{"page": "https://ix.dev/explore/schemas.json"}'

The page lives in indexable-inc/ix (packages/web/src/lib/explore/schema.ts, `SCHEMAS`) and
deploys from ix's main; the daemon's `hello` sends blaze-profile's `schema::VERSION`
(crates/blaze-profile/src/schema.rs). A page that does not read the daemon's version refuses
the session, so a blaze whose version the deployed page lacks must not land or be released:
the page moves first. On 2026-11-05 blaze 0.20261105.1 wrote schema 5 while the page read only
1, and `blaze explore` showed nothing but that refusal.

Exits 0 when the page reads this tree's version, 1 naming both and what to do otherwise, and 2
when the page cannot be read (the step, the URL and the error). No retry and no pass on a
failed fetch: a gate that cannot see the page does not say it is fine.
"""

import asyncio
import json
import re
import sys
import urllib.request
from dataclasses import dataclass
from pathlib import Path

from typed_input import parse

ROOT = Path(__file__).resolve().parents[2]
SCHEMA_RS = ROOT / "crates/blaze-profile/src/schema.rs"
PAGE_SOURCE = "indexable-inc/ix packages/web/src/lib/explore/schema.ts (SCHEMAS)"


@dataclass(frozen=True)
class Check:
    # Where the deployed page lists the schemas it reads: `{"reads": [n, ...]}`.
    page: str = "https://ix.dev/explore/schemas.json"
    timeout_s: float = 30.0


def tree_version() -> int:
    text = SCHEMA_RS.read_text()
    found = re.search(r"^pub const VERSION: u32 = (\d+);", text, re.MULTILINE)
    if found is None:
        raise SystemExit(f"explore_page.py: no `pub const VERSION: u32 = N;` in {SCHEMA_RS}")
    return int(found.group(1))


def fetch(url: str, timeout_s: float) -> bytes:
    request = urllib.request.Request(url, headers={"user-agent": "blaze explore_page.py"})
    with urllib.request.urlopen(request, timeout=timeout_s) as response:
        return response.read()


async def page_reads(check: Check) -> list[int]:
    try:
        async with asyncio.timeout(check.timeout_s + 5):
            body = await asyncio.to_thread(fetch, check.page, check.timeout_s)
    except Exception as e:
        print(f"explore_page.py: fetching {check.page} failed: {e!r}", file=sys.stderr)
        raise SystemExit(2) from e
    try:
        value = json.loads(body)
        reads = value["reads"]
        if not isinstance(reads, list) or not all(isinstance(it, int) for it in reads):
            raise ValueError(f"`reads` is {reads!r}, not a list of integers")
    except (ValueError, KeyError, TypeError) as e:
        print(f"explore_page.py: {check.page} is not {{\"reads\": [n, ...]}}: {e}: {body[:200]!r}", file=sys.stderr)
        raise SystemExit(2) from e
    return reads


def main() -> int:
    check = parse(Check, sys.argv[1] if len(sys.argv) > 1 else "{}", "explore_page.py")
    version = tree_version()
    reads = asyncio.run(page_reads(check))
    if version in reads:
        print(f"explore_page.py: {check.page} reads schemas {reads}, which has this tree's {version}")
        return 0
    print(
        f"explore_page.py: this tree writes blaze-profile schema {version} ({SCHEMA_RS.relative_to(ROOT)}), "
        f"and the deployed explorer ({check.page}) reads only {reads}: a daemon of this tree would be "
        f"refused by ix.dev/explore. Add {version} to {PAGE_SOURCE}, with whatever the page must read "
        f"differently, land it in ix so its publish deploys ix-web, then rerun this job.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
