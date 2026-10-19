#!/usr/bin/env python3
"""Fail on any npm advisory unless it is listed, unexpired, in the allow file.

usage: npm-audit-gate.py AUDIT_JSON ALLOW_JSON

AUDIT_JSON is `npm audit --json` output. ALLOW_JSON maps an advisory id
(GHSA-...) to {"expires": "YYYY-MM-DD", "reason": "..."}. An exception covers a
known, accepted exposure while the real fix lands; once it expires the build
fails again.
"""

import datetime
import json
import sys


def advisories(audit):
    found = {}
    for vulnerability in audit.get("vulnerabilities", {}).values():
        for via in vulnerability.get("via", []):
            if isinstance(via, dict) and via.get("url"):
                found[via["url"].rstrip("/").rsplit("/", 1)[-1]] = via.get("title", "")
    return found


def main(audit_path, allow_path):
    with open(audit_path) as handle:
        audit = json.load(handle)
    if "error" in audit or "vulnerabilities" not in audit:
        print(f"npm audit did not produce a report: {audit.get('error', audit)}", file=sys.stderr)
        return 1
    with open(allow_path) as handle:
        allowed = json.load(handle)
    today = datetime.date.today()
    failures = []
    for advisory, title in sorted(advisories(audit).items()):
        entry = allowed.get(advisory)
        if entry is None:
            failures.append(f"{advisory}: {title}")
        elif datetime.date.fromisoformat(entry["expires"]) < today:
            failures.append(f"{advisory}: exception expired {entry['expires']} ({title})")
        else:
            print(f"allowed until {entry['expires']}: {advisory} ({entry['reason']})")
    for failure in failures:
        print(f"npm advisory: {failure}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main(*sys.argv[1:3]))
