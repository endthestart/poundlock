# Third-party notices

Poundlock is licensed under Apache-2.0. Its dependencies and bundled assets keep
their own licenses. This file says where each one's full text is.

## Rust crates

Native release archives and the container image include `THIRD_PARTY_LICENSES.md`,
generated from the locked Cargo dependency graph for that build target. The build
fails if a crate has no license text. `licenses/sources.json` records the exact
upstream sources for texts missing from a crate's own archive.

## Fonts and theme compiled into the binary

The web console's fonts and colours are compiled into the `poundlock` binary, so
every native archive and the image carry them. `THIRD_PARTY_LICENSES.md` repeats
these texts in full.

- Inter, copyright 2016 The Inter Project Authors, under the SIL Open Font License
  1.1: `web/fonts/INTER-LICENSE`.
- JetBrains Mono, copyright 2020 The JetBrains Mono Project Authors, under the SIL
  Open Font License 1.1: `web/fonts/JETBRAINS-MONO-LICENSE`.
- The colour palette is derived from Catppuccin, copyright 2021 Catppuccin, under
  the MIT License: `docs/design/CATPPUCCIN-LICENSE`.

The fonts remain under the OFL; Poundlock's Apache-2.0 license does not apply to them.

## Container image

The image includes Pi 1.1.0 (`@earendil-works/pi-coding-agent` and its
`@earendil-works/*` packages), licensed under MIT, copyright Mario Zechner. Its
npm packages ship no license file, so the upstream license is retained in
`licenses/pi-1.1.0-LICENSE`, retrieved from the `v1.1.0` commit
`abe508e1b89912adde45528136c3221eb69acdd7` of
[earendil-works/pi](https://github.com/earendil-works/pi).

Pi also bundles HTML-export copies of Highlight.js 11.9.0 (BSD-3-Clause) and
Marked 18.0.5 (MIT). Their full texts are in `licenses/`, and
`licenses/pi-vendored-sources.json` records the package sources and text hashes.

`/usr/share/doc/poundlock/` in the image holds `LICENSE`, `NOTICE`, this file,
`THIRD_PARTY_LICENSES.md` (Rust crates and embedded assets) and
`RUNTIME_LICENSES.md`, generated from the JavaScript packages actually installed.
That build also fails on a missing license text; `licenses/runtime-sources.json`
records upstream sources and hashes for texts omitted from npm archives. Pi's
optional desktop clipboard integration is not installed. The image is built on
Debian; Debian package copyright and license records are under `/usr/share/doc`,
and their sources are available from Debian's archive. Poundlock's license does
not apply to those packages.

## Documentation

`docs/maps/poundlock-release.architecture.html` was generated with Archify
2.17.0-dev.1 and contains its page script, licensed under MIT, copyright 2026
tt-a1i (Archify) and 2025 Cocoon AI: `licenses/archify-2.17.0-LICENSE`.

## Not included

Poundlock does not distribute model weights or inference servers. Obtain them from
their publishers and follow their licenses.
