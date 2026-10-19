"""Turn a Poundlock Toolkit recommendation summary into a capability evidence block.

    poundlock-toolkit recommend --format json RESULT.json [...] --output summary.json
    python3 -m pip install 'jsonschema>=4.23,<5'
    python3 scripts/toolkit-capabilities.py summary.json > block.json

Paste the output into a model's Capability evidence field in Models.

The input is Toolkit's versioned summary, `urn:poundlock-toolkit:recommendation:1`
(Poundlock Toolkit 0.5.0 or later). Toolkit decides every level, count and pairing;
this script only maps those verdicts onto Poundlock's capability contract,
`docs/reference/fixtures/capability-measurement.schema.json`, and validates each
entry against it before printing. It does not import Toolkit, and it is read-only
and network-free.

One entry per applicable result: `code_repair_verified` from a run that planned
coding tasks, `plan_tool_schema` from a tool-schema run, and `sealed_investigation`
from each investigation pair (both variants against one configuration). The
summary carries no endpoint, credential or free-form field, so neither does the
block.
"""

import hashlib
import json
import sys
from pathlib import Path

from jsonschema import Draft202012Validator, FormatChecker

SCHEMA = (
    Path(__file__).resolve().parents[1]
    / "docs/reference/fixtures/capability-measurement.schema.json"
)

#: The only summary shape this script reads. Version 1 only gains optional fields.
SUMMARY = "urn:poundlock-toolkit:recommendation:1"
QUALIFICATION = "urn:poundlock-toolkit:qualification:1"
TOOL_SCHEMA = "urn:poundlock-toolkit:tool-schema:1"

#: `CAPABILITY_SOURCE_TOOLKIT` in src/domain.rs. The controller refuses a
#: measurement that does not say where it came from.
CAPABILITY_SOURCE = "poundlock_toolkit"

LEVEL_STATUS = {
    "repeated task qualification": "qualified",
    "observed smoke pass": "smoke",
    "insufficient evidence": "insufficient",
}


def reasoning(sent, run_id):
    """Poundlock profiles know two thinking defaults: `model_default` sends
    nothing, `off` disables it. Toolkit's `reasoning_sent` says which one the
    measured requests matched; an explicit effort level matches neither."""
    if sent == "omitted":
        return "model_default"
    if sent == "disabled":
        return "off"
    raise ValueError(
        f"result {run_id} sent reasoning effort {sent!r}; a Poundlock profile is "
        "measured with thinking off or at the model's default"
    )


def measured_settings(row, sent=None):
    s = row["settings"]
    if s is None:
        raise ValueError(f"result {row['run_id']} records no settings")
    if sent is None:
        sent = s["reasoning_sent"]
    return {
        "context_tokens": s["context_tokens"],
        "max_output_tokens": s["max_output_tokens"],
        "reasoning": reasoning(sent, row["run_id"]),
        "harness": row["harness"],
    }


def tool_schema_sent(row):
    """A tool-schema run may use both boundaries, and each reports what it sent."""
    sent = {b["reasoning_sent"] for b in row["tool_schema"]["boundaries"]}
    if len(sent) != 1:
        raise ValueError(
            f"result {row['run_id']} measured different thinking behaviour on each "
            "API; measure one boundary per run to make capability evidence"
        )
    return sent.pop()


def entry(row, **fields):
    value = {
        "source": CAPABILITY_SOURCE,
        "toolkit_version": row["toolkit_version"],
        "measured_at": row["created_at"],
        **fields,
    }
    if row["identity_sha256"]:
        value["identity_sha256"] = row["identity_sha256"]
    return value


def tool_schema_status(row):
    """Every planned trial attempted and passed; the run's own mode then decides
    whether that is a smoke pass or a qualification."""
    boundaries = row["tool_schema"]["boundaries"]
    planned = sum(b["planned"] for b in boundaries)
    attempted = sum(b["attempted"] for b in boundaries)
    passed = sum(b["passed"] for b in boundaries)
    if attempted == 0 or attempted != planned or passed != attempted:
        return "insufficient", passed, attempted
    if row["tool_schema"]["mode"] == "qualification":
        return "qualified", passed, attempted
    return "smoke", passed, attempted


def paired_digest(*hashes):
    """One evidence hash for a measurement scored from several results: the
    SHA-256 of their ordered digests as compact JSON. `run_ids` names the runs."""
    raw = json.dumps(list(hashes), separators=(",", ":")).encode()
    return hashlib.sha256(raw).hexdigest()


def capability_block(summary):
    if not isinstance(summary, dict) or summary.get("schema") != SUMMARY:
        raise ValueError(
            f"expected a Toolkit summary ({SUMMARY}); make one with "
            "`poundlock-toolkit recommend --format json`"
        )
    rows = {row["run_id"]: row for row in summary["results"]}
    capabilities = []

    for row in summary["results"]:
        if row["result_schema"] == QUALIFICATION:
            coding = next(w for w in row["workloads"] if w["workload"] == "coding")
            if coding["planned"] == 0:
                continue
            capabilities.append(
                entry(
                    row,
                    name="code_repair_verified",
                    status=LEVEL_STATUS[coding["level"]],
                    passed=coding["passed"],
                    attempted=coding["attempted"],
                    run_ids=[row["run_id"]],
                    result_sha256=row["result_sha256"],
                    measured_settings=measured_settings(row),
                )
            )
        elif row["result_schema"] == TOOL_SCHEMA:
            status, passed, attempted = tool_schema_status(row)
            capabilities.append(
                entry(
                    row,
                    name="plan_tool_schema",
                    status=status,
                    passed=passed,
                    attempted=attempted,
                    run_ids=[row["run_id"]],
                    result_sha256=row["result_sha256"],
                    measured_settings=measured_settings(row, tool_schema_sent(row)),
                )
            )
        else:
            raise ValueError(f"result {row['run_id']} has an unknown schema")

    for pair in summary["investigation_pairs"]:
        findable = rows[pair["findable_run_id"]]
        absent = rows[pair["absent_run_id"]]
        if pair["qualified"]:
            status = "qualified"
        elif "insufficient evidence" not in (
            pair["findable"]["level"],
            pair["absent"]["level"],
        ):
            status = "smoke"
        else:
            status = "insufficient"
        capabilities.append(
            entry(
                findable,
                name="sealed_investigation",
                status=status,
                # Paired, never summed: summing lets a model that always answers
                # "not satisfied" (every `absent` attempt passes, every
                # `findable` one fails) report a majority of passes. The weaker
                # variant decides.
                passed=min(pair["findable"]["passed"], pair["absent"]["passed"]),
                attempted=max(
                    pair["findable"]["attempted"], pair["absent"]["attempted"]
                ),
                run_ids=[findable["run_id"], absent["run_id"]],
                result_sha256=paired_digest(
                    findable["result_sha256"], absent["result_sha256"]
                ),
                measured_at=max(findable["created_at"], absent["created_at"]),
                measured_settings=measured_settings(findable),
            )
        )

    validator = Draft202012Validator(
        json.loads(SCHEMA.read_text()), format_checker=FormatChecker()
    )
    for capability in capabilities:
        validator.validate(capability)
        if capability["passed"] > capability["attempted"]:
            raise ValueError("a capability cannot pass more attempts than it made")
    return capabilities


def main(paths):
    if len(paths) != 1:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    summary = json.loads(Path(paths[0]).read_text())
    block = {"capabilities": capability_block(summary)}
    print(json.dumps(block, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
