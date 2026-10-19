# Base images are pinned by digest (L18); bump the tag and digest together.
FROM rust:1.98-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e AS builder
# Base64 Ed25519 public key that node binaries embed to verify signed updates
# (docs/node-update-signing.md). Empty builds nodes that accept checksummed
# updates over HTTPS or loopback only.
ARG POUNDLOCK_UPDATE_PUBLIC_KEY=
WORKDIR /source
COPY Cargo.toml Cargo.lock ./
COPY migrations ./migrations
COPY config ./config
COPY src ./src
COPY web ./web
COPY deploy ./deploy
COPY docs/linux-node.md ./docs/linux-node.md
COPY docs/reference/openapi.yaml ./docs/reference/openapi.yaml
COPY docs/reference/fixtures ./docs/reference/fixtures
# Contract tests assert the advertised verifier toolchain matches this file.
COPY Dockerfile ./Dockerfile
COPY tests/*.rs ./tests/
COPY scripts ./scripts
COPY licenses ./licenses
COPY docs/design ./docs/design
COPY LICENSE NOTICE ./
ENV POUNDLOCK_BUILD_LINKER=bfd
RUN apt-get update && apt-get install --yes --no-install-recommends binutils python3 \
    && rm -rf /var/lib/apt/lists/* \
    && rustup component add rustfmt clippy \
    && bash scripts/build-binary.sh \
    && cp dist/*/THIRD_PARTY_LICENSES.md target/release/THIRD_PARTY_LICENSES.md

# Independent-check toolchain only. Durable jobs reuse this stage; do not bake
# Ruff, mypy, or pytest — the snapshot lockfile supplies those.
FROM node:22-bookworm-slim@sha256:c3de60bf2f9dd0ac6370e6117950ff62d6e339527e7472301c9c78a017978392 AS verifier-toolchain
# The npm bundled with the base image carries advisories in its own dependencies.
ARG NPM_VERSION=11.21.1
COPY --from=ghcr.io/astral-sh/uv:0.12.24@sha256:3af4716e991d6956a41e573eab705d0ee08500cd829ed30293eb8472f372c65a /uv /usr/local/bin/uv
ENV UV_PYTHON_INSTALL_DIR=/opt/python
ENV PATH=/usr/local/bin:/usr/local/sbin:/usr/bin:/usr/sbin:/bin:/sbin
# The base is pinned by digest; Debian's security updates are applied here so a
# fixed package does not wait for the next upstream rebuild.
RUN apt-get update \
    && apt-get upgrade --yes --no-install-recommends \
    && apt-get install --yes --no-install-recommends ca-certificates python3 python3-venv python3-pip \
    && rm -rf /var/lib/apt/lists/* \
    && chmod 755 /usr/local/bin/uv \
    && uv python install 3.14 \
    && ln -sf "$(uv python find 3.14)" /usr/local/bin/python3.14 \
    # Its own pip vendors msgpack, urllib3 and setuptools' pkg_resources, and nothing
    # uses it: projects use uv, and `python3.14 -m venv` bootstraps pip from ensurepip.
    && python3.14 -m pip uninstall --yes --break-system-packages pip \
    && ! python3.14 -m pip --version 2>/dev/null \
    && chmod -R a+rX /opt/python \
    && python3.14 --version \
    && uv --version \
    && npm install --global --ignore-scripts "npm@${NPM_VERSION}" \
    && npm cache clean --force \
    && test "$(npm --version)" = "${NPM_VERSION}"

FROM verifier-toolchain
ARG PI_VERSION=1.1.0
# Pi and everything it depends on come from pi-runtime/package-lock.json.
COPY pi-runtime/package.json pi-runtime/package-lock.json /opt/poundlock/pi/
COPY scripts/runtime-licenses.py /tmp/poundlock-notices/scripts/runtime-licenses.py
COPY licenses/ /tmp/poundlock-notices/licenses/
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl git passwd ripgrep python3 python3-venv python3-pip \
    && rm -rf /var/lib/apt/lists/* \
    && npm ci --prefix /opt/poundlock/pi --ignore-scripts --omit=optional --omit=dev \
    && test "$(node -p 'require("/opt/poundlock/pi/node_modules/@earendil-works/pi-coding-agent/package.json").version')" = "${PI_VERSION}" \
    && npm --version \
    && ln -s /opt/poundlock/pi/node_modules/.bin/pi /usr/local/bin/pi \
    && python3 /tmp/poundlock-notices/scripts/runtime-licenses.py \
       /opt/poundlock/pi /usr/share/doc/poundlock/RUNTIME_LICENSES.md \
    && rm -rf /tmp/poundlock-notices \
    && groupadd --gid 10001 poundlock \
    && useradd --uid 10001 --gid 10001 --no-create-home poundlock \
    && mkdir -p /data /run/poundlock /run/poundlock-wake /work /trusted /verify /home/worker /projects /results \
    && chown 10001:10001 /run/poundlock-wake && chmod 700 /run/poundlock-wake \
    && chown -R 10001:10001 /data /run/poundlock /work /projects /results
COPY --from=builder /source/target/release/poundlock /usr/local/bin/poundlock
# The controller serves node binaries for self-update from here: the Linux
# binary built above, plus any natively built binaries placed in node-artifacts/
# (release CI adds the macOS and other Linux ones). Nothing here is signed; see
# docs/node-update-signing.md for signing your own builds.
COPY --from=builder /source/dist /opt/poundlock/nodes
COPY node-artifacts/ /opt/poundlock/nodes/
COPY LICENSE NOTICE THIRD_PARTY_NOTICES.md /usr/share/doc/poundlock/
COPY --from=builder /source/target/release/THIRD_PARTY_LICENSES.md /usr/share/doc/poundlock/THIRD_PARTY_LICENSES.md
ENV POUNDLOCK_CLIENT_ARTIFACTS=/opt/poundlock/nodes
USER 10001:10001
ENTRYPOINT ["poundlock"]
