#!/usr/bin/env bash
set -euo pipefail
# Bytecode goes to a throwaway folder instead of __pycache__ in the tree;
# py_compile ignores PYTHONDONTWRITEBYTECODE, but not this.
PYTHONPYCACHEPREFIX=$(mktemp -d)
export PYTHONPYCACHEPREFIX
trap 'rm -rf "$PYTHONPYCACHEPREFIX"' EXIT
for script in scripts/*.sh deploy/linux/*.sh node-runtimes/llamacpp/deploy/*.sh; do bash -n "$script"; done
# ShellCheck comes from scripts/requirements-ci.txt.
shellcheck scripts/*.sh deploy/linux/*.sh node-runtimes/llamacpp/deploy/*.sh
for script in web/*.js tests/uat/*.cjs; do node --check "$script"; done
python3 -m py_compile scripts/*.py tests/uat/linux-node.py
for fixture in docs/reference/fixtures/*.json; do python3 -m json.tool "$fixture" >/dev/null; done
python3 scripts/theme.py --check
grep -Fq 'COPY docs/reference/openapi.yaml ./docs/reference/openapi.yaml' Dockerfile
python3 scripts/check-github-release-workflow.py
# Ruff comes from scripts/requirements-ci.txt; the runtime's pyproject configures it.
ruff check --no-cache node-runtimes/llamacpp
ruff format --no-cache --check node-runtimes/llamacpp
# Strict mypy (the runtime's own config) on its source; its tests stay loosely typed.
(cd node-runtimes/llamacpp && mypy --cache-dir=/dev/null src)
python3 -m unittest discover -s node-runtimes/llamacpp/tests -t node-runtimes/llamacpp
python3 -m py_compile tests/e2e/*.py
# Needs jsonschema (scripts/requirements-ci.txt).
python3 -m unittest scripts/test_toolkit_capabilities.py
python3 -m unittest scripts/test_dev_version.py
