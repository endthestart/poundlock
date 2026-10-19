#!/usr/bin/env bash
set -euo pipefail
for script in scripts/*.sh deploy/linux/*.sh node-runtimes/llamacpp/deploy/*.sh; do bash -n "$script"; done
for script in web/*.js tests/uat/*.cjs; do node --check "$script"; done
python3 -m py_compile scripts/*.py tests/uat/linux-node.py
for fixture in docs/reference/fixtures/*.json; do python3 -m json.tool "$fixture" >/dev/null; done
python3 scripts/theme.py --check
grep -Fq 'COPY docs/reference/openapi.yaml ./docs/reference/openapi.yaml' Dockerfile
python3 scripts/check-github-release-workflow.py
python3 -m unittest discover -s node-runtimes/llamacpp/tests -t node-runtimes/llamacpp
python3 -m py_compile tests/e2e/*.py
# Needs jsonschema (scripts/requirements-ci.txt).
python3 -m unittest scripts/test_toolkit_capabilities.py
