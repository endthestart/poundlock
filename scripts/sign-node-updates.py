#!/usr/bin/env python3
"""Sign the node binaries of a release image, and upload the signatures to its controller.

    POUNDLOCK_UPDATE_SIGNING_KEY=... python3 scripts/sign-node-updates.py \\
        https://controller.example ADMIN_TOKEN_FILE \\
        --image REGISTRY/poundlock@sha256:DIGEST --version 0.2.0 [--public-key BASE64]

Run on the release signer's own machine after each deploy; the private key never
goes to CI or the controller. The key is an Ed25519 PKCS#8 PEM, or that PEM
base64-encoded on one line (the form kept in the macOS Keychain), read from the
environment and never written to disk.

What is signed comes from the image, not from the controller: the binaries are
copied out of the digest-pinned image and hashed here, and the version is the
release's. The controller's own listing must name exactly those hashes and that
version, or nothing is signed, so a controller someone else controls cannot get
its choice of binary signed. For each binary this signs
{"os","arch","version","sha256"}, checks the signature, and uploads manifest and
signature to /api/v1/nodes/update-signatures. Needs Docker and OpenSSL 3 or
later. See docs/node-update-signing.md.
"""
import argparse
import base64
import hashlib
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.parse
import urllib.request


def fail(message: str) -> None:
    sys.exit(f"sign-node-updates: {message}")


def openssl() -> str:
    for candidate in ("/opt/homebrew/opt/openssl@3/bin/openssl", "/usr/local/opt/openssl@3/bin/openssl",
                      shutil.which("openssl") or ""):
        if candidate and os.path.exists(candidate):
            version = subprocess.run([candidate, "version"], capture_output=True, text=True).stdout
            if version.startswith(("OpenSSL 3.", "OpenSSL 4.")):
                return candidate
    fail("OpenSSL 3 or later is required (Homebrew openssl@3 on macOS)")
    return ""


def private_pem(raw: str) -> bytes:
    raw = raw.strip()
    if raw.startswith("-----BEGIN"):
        return raw.encode() + b"\n"
    try:
        return base64.b64decode("".join(raw.split()), validate=True)
    except ValueError:
        fail("POUNDLOCK_UPDATE_SIGNING_KEY is neither a PEM key nor base64")
    return b""


def call(base: str, token: str, method: str, path: str, body=None):
    data = None if body is None else json.dumps(body).encode()
    request = urllib.request.Request(base.rstrip("/") + path, data=data, method=method, headers={
        "Authorization": f"Bearer {token}", "Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            return json.load(response)
    except urllib.error.HTTPError as error:
        fail(f"{method} {path} answered {error.code}: {error.read(2000).decode(errors='replace')}")


def image_binaries(image: str, docker: list[str]) -> dict[tuple[str, str], str]:
    """SHA-256 of each node binary the controller in `image` serves, by (os, arch).

    linux-x86_64 is the controller's own executable; the rest are under
    /opt/poundlock/nodes/<os>-<arch>/poundlock.
    """
    created = subprocess.run(docker + ["create", "--platform", "linux/amd64", image],
                             capture_output=True, text=True)
    if created.returncode != 0:
        fail(f"could not create a container from {image}: {created.stderr.strip()[-300:]}")
    container = created.stdout.strip()
    try:
        hashes = {}
        own = subprocess.run(docker + ["cp", f"{container}:/usr/local/bin/poundlock", "-"],
                             capture_output=True, check=True).stdout
        for name, data in tar_files(own):
            hashes[("linux", "x86_64")] = hashlib.sha256(data).hexdigest()
        nodes = subprocess.run(docker + ["cp", f"{container}:/opt/poundlock/nodes", "-"],
                               capture_output=True, check=True).stdout
        for name, data in tar_files(nodes):
            match = re.fullmatch(r"nodes/(linux|macos)-(x86_64|aarch64)/poundlock", name)
            if match and (match[1], match[2]) != ("linux", "x86_64"):
                hashes[(match[1], match[2])] = hashlib.sha256(data).hexdigest()
        return hashes
    finally:
        subprocess.run(docker + ["rm", container], capture_output=True)


def tar_files(archive: bytes):
    with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
        for member in tar.getmembers():
            if member.isfile():
                yield member.name, tar.extractfile(member).read()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("controller")
    parser.add_argument("admin_token_file")
    parser.add_argument("--image", required=True, help="the deployed image, pinned by digest")
    parser.add_argument("--version", required=True, help="the release's version, from its tag")
    parser.add_argument("--public-key", default="")
    parser.add_argument("--docker-context", default="")
    args = parser.parse_args()
    url = urllib.parse.urlparse(args.controller)
    if url.scheme != "https" and url.hostname not in ("localhost", "127.0.0.1", "::1"):
        fail("the controller must be reached over https://")
    if not re.fullmatch(r"\S+@sha256:[0-9a-f]{64}", args.image):
        fail("--image must be pinned by digest (NAME@sha256:...)")
    if not re.fullmatch(r"\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?", args.version):
        fail(f"invalid version: {args.version}")
    key = os.environ.get("POUNDLOCK_UPDATE_SIGNING_KEY")
    if not key:
        fail("POUNDLOCK_UPDATE_SIGNING_KEY is not set")
    pem = private_pem(key)
    tool = openssl()
    token = open(args.admin_token_file).read().strip()
    with tempfile.TemporaryDirectory() as work:
        public_pem = subprocess.run([tool, "pkey", "-pubout"], input=pem, capture_output=True).stdout
        if not public_pem:
            fail("POUNDLOCK_UPDATE_SIGNING_KEY is not a PEM private key")
        der = subprocess.run([tool, "pkey", "-pubin", "-outform", "DER"], input=public_pem,
                             capture_output=True).stdout
        public = base64.b64encode(der[-32:]).decode()
        if len(der) != 44:
            fail("POUNDLOCK_UPDATE_SIGNING_KEY is not an Ed25519 key")
        if args.public_key and args.public_key != public:
            fail("the signing key does not match --public-key")
        public_path = os.path.join(work, "public.pem")
        open(public_path, "wb").write(public_pem)
        docker = ["docker"] + (["--context", args.docker_context] if args.docker_context else [])
        expected = image_binaries(args.image, docker)
        if not expected:
            fail(f"{args.image} holds no node binaries")
        listing = call(args.controller, token, "GET", "/api/v1/nodes/binaries")
        version = args.version
        if listing["version"] != version:
            fail(f"the controller serves {listing['version']}, not {version}; deploy {args.image} first")
        served = {(p["os"], p["arch"]): p["sha256"] for p in listing["platforms"] if p.get("available")}
        if served != expected:
            fail("the controller's binaries are not the image's; nothing was signed")
        uploads = []
        for (os_name, arch), sha256 in sorted(expected.items()):
            platform = {"os": os_name, "arch": arch, "sha256": sha256}
            manifest = json.dumps({"os": os_name, "arch": arch, "version": version,
                                   "sha256": sha256}, separators=(",", ":"))
            manifest_path = os.path.join(work, "manifest.json")
            open(manifest_path, "w").write(manifest)
            # The key reaches openssl on a pipe, never as a file.
            signed = subprocess.run([tool, "pkeyutl", "-sign", "-rawin", "-inkey", "/dev/stdin",
                                     "-in", manifest_path], input=pem, capture_output=True)
            if signed.returncode != 0 or len(signed.stdout) != 64:
                fail(f"could not sign the manifest for {platform['os']}/{platform['arch']}")
            signature_path = os.path.join(work, "signature")
            open(signature_path, "wb").write(signed.stdout)
            checked = subprocess.run([tool, "pkeyutl", "-verify", "-rawin", "-pubin", "-inkey", public_path,
                                      "-in", manifest_path, "-sigfile", signature_path], capture_output=True)
            if checked.returncode != 0:
                fail(f"the signature for {platform['os']}/{platform['arch']} does not verify")
            uploads.append({"manifest": manifest, "signature": base64.b64encode(signed.stdout).decode()})
            print(f"sign-node-updates: signed {platform['os']}-{platform['arch']} {version} {platform['sha256']}")
        if not uploads:
            fail("the controller lists no node binaries")
        accepted = call(args.controller, token, "PUT", "/api/v1/nodes/update-signatures", uploads)
        print(f"sign-node-updates: uploaded {len(accepted['accepted'])} signatures; public key {public}")


if __name__ == "__main__":
    main()
