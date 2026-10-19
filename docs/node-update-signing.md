# Node update signing

A node that updates itself runs whatever binary it installs, as its own user. Who
decides which binary that is depends on how the node was built.

## Public release binaries: unsigned

The node binaries in Poundlock's GitHub releases and container image embed no
update key and carry no signatures. A node built this way trusts the controller:
it installs a binary only when the controller offers a newer version, only over
`https://` or loopback, and only when the download matches the SHA-256 the
controller states. Whoever controls your controller, or its TLS identity, chooses
the code those nodes run.

That is the right default for most installs, where the controller and the nodes
belong to the same operator. If you need nodes to refuse code your controller did
not sign, build your own binaries with your own key, as below. A project-held
release key may be added later; until then there is no Poundlock key to trust.

Set `auto_update = false` in `node.toml` to install every update by hand instead.

## What a signed build checks

When a build embeds a public key, the controller serves two files beside each
binary `<os>-<arch>/poundlock`:

- `poundlock.manifest.json`, the exact signed bytes:
  `{"os":"linux","arch":"x86_64","version":"0.3.0","sha256":"<hex>"}`;
- `poundlock.manifest.sig`, the base64 Ed25519 signature over those bytes.

The controller relays both and does not interpret the signature. Before it
downloads or runs anything, a node built with a key requires:

- a signature that verifies with its embedded key;
- a manifest for its own OS and processor;
- a version newer than its own.

It then requires that the download matches the signed SHA-256, and that the new
binary's `node-check` passes and reports the same key. A missing or invalid
signature, an older or equal version, or a mismatched hash leaves the node on its
current binary, and the node logs why. A replacement that fails its check is not
installed. An installed update keeps the binary it replaced beside it as
`poundlock.previous`, for a manual rollback.

## Signing your own builds

You need OpenSSL 3 (`openssl version` must not say LibreSSL) and a place to keep
the private key that is not your controller and not a CI secret. The commands
below keep it in the macOS login Keychain; any equivalent offline store works.

1. Generate the key pair without writing the private key to disk, and print the
   public key:

   ```sh
   key=$(openssl genpkey -algorithm ed25519 | openssl base64 -A)
   security add-generic-password -a "$USER" -s poundlock-update-signing-key \
     -l "Poundlock node update signing key" -T "" -w "$key"
   printf '%s' "$key" | openssl base64 -d -A | openssl pkey -pubout -outform DER \
     | tail -c 32 | openssl base64 -A; echo
   unset key
   ```

   The last command prints the public key: 44 characters ending in `=`.
2. Build with the public key embedded. For native binaries:
   `POUNDLOCK_UPDATE_PUBLIC_KEY=<public key> bash scripts/build-binary.sh`.
   For the image: `docker build --build-arg POUNDLOCK_UPDATE_PUBLIC_KEY=<public key> .`
   Only the public key enters the build.
3. Deploy that build and install it on each node once by hand: a node built
   without your key will not accept your signatures as a reason to switch.
4. After each deploy, sign on the machine that holds the key and upload the
   signatures to the controller:

   ```sh
   POUNDLOCK_UPDATE_SIGNING_KEY="$(security find-generic-password -a "$USER" \
     -s poundlock-update-signing-key -w)" \
     python3 scripts/sign-node-updates.py https://CONTROLLER ADMIN_TOKEN_FILE \
     --image REGISTRY/poundlock@sha256:<deployed digest> --version <release> \
     --public-key <public key>
   ```

   It copies the node binaries out of the deployed image, pulled by digest, and
   hashes them locally. If the controller lists other hashes or another version,
   it signs nothing. It then signs each binary, checks every signature against the
   public key and uploads them; `GET /api/v1/nodes/binaries` then reports
   `"signed": true`. The controller keeps a signature only for a binary it serves
   at its own version, so the administrator token alone cannot make a signed node
   install anything.

To sign binaries in a local build folder instead, for example before copying them
into `node-artifacts/` for your own image build:

```sh
POUNDLOCK_UPDATE_SIGNING_KEY="$(security find-generic-password -a "$USER" \
  -s poundlock-update-signing-key -w)" bash scripts/sign-node-manifests.sh DIR VERSION
```

From then on a node's technical report shows `"update_verification": "signature"`.
A deploy you never sign leaves those nodes on their current version.

## Rotation and loss

A node trusts exactly the key it was built with; an old key cannot vouch for a
new one. To rotate, or after the key is lost or exposed, generate a new pair and
reinstall every node by hand with a build that embeds it. Until then, set
`auto_update = false` on nodes that trust an exposed key. Back the private key up
the way you back up other credentials you cannot recreate.
