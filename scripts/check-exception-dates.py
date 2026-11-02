#!/usr/bin/env python3
"""Fail when a cargo-deny exception has no expiry date or its date has passed.

usage: check-exception-dates.py DENY_TOML

cargo-deny has no expiry of its own. Every entry in `advisories.ignore`,
`licenses.exceptions`, `bans.skip` and `bans.skip-tree` must be an inline table
on its own line, and its `reason` must contain `expires: YYYY-MM-DD`. The array
must close with a line holding only `]`. The day itself still counts as valid,
as in npm-audit-gate.py; the build fails the day after.

Runs on the CI job image's Python 3.10, which has no tomllib, so the file is
read line by line.
"""

import datetime
import re
import sys

EXCEPTION_ARRAYS = ("ignore", "exceptions", "skip", "skip-tree")
OPEN = re.compile(r"^(%s)\s*=\s*\[(.*)$" % "|".join(EXCEPTION_ARRAYS))
EXPIRES = re.compile(r"expires: (\d{4}-\d{2}-\d{2})")


def entries(lines):
    """Yield (line number, array name, entry text) for every exception entry."""
    array = None
    for number, raw in enumerate(lines, 1):
        line = raw.strip()
        if array is None:
            opened = OPEN.match(line)
            if opened is None:
                continue
            array, line = opened.group(1), opened.group(2).strip()
        if line.startswith("#") or not line:
            continue
        if line == "]":
            array = None
        else:
            yield number, array, line.rstrip(",")


def main(path):
    with open(path, encoding="utf-8") as handle:
        lines = handle.read().splitlines()
    today = datetime.date.today()
    problems = []
    count = 0
    for number, array, entry in entries(lines):
        count += 1
        where = f"{path}:{number} ({array})"
        if not entry.startswith("{") or "reason" not in entry:
            problems.append(f"{where}: needs a one-line inline table with a reason")
            continue
        found = EXPIRES.search(entry)
        if found is None:
            problems.append(f"{where}: reason needs 'expires: YYYY-MM-DD'")
        elif datetime.date.fromisoformat(found.group(1)) < today:
            problems.append(f"{where}: exception expired {found.group(1)}")
        else:
            print(f"allowed until {found.group(1)}: {entry[:100]}")
    for problem in problems:
        print(f"cargo-deny exception: {problem}", file=sys.stderr)
    print(f"{count} cargo-deny exception(s) checked")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
