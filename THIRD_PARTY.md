# Third-party licenses

Codehaven is MIT licensed (see LICENSE). It builds on the software below. Every dependency in
`Cargo.lock` (about 580 crates, checked with `cargo metadata` on 2026-11-22) is under a permissive
license that allows redistribution under MIT. This file lists what needs a word.

## Rust crates

Almost all crates are `MIT`, `Apache-2.0`, or `MIT OR Apache-2.0`. The rest are also permissive:

- BSD-2-Clause and BSD-3-Clause: curve25519-dalek, ed25519-dalek, subtle, encoding_rs (with its
  own Mozilla-style data terms), matchit.
- ISC and Zlib: ring (Apache-2.0 AND ISC), rustls-webpki, untrusted, foldhash, zlib-rs.
- Unicode-3.0: the unicode-* data crates.
- CDLA-Permissive-2.0: webpki-roots (the Mozilla root certificate list).
- MPL-2.0: cssparser and dtoa-short. MPL-2.0 is a file-level license. Codehaven does not modify
  these crates. If you redistribute a build, the source of those two crates stays available from
  crates.io under MPL-2.0.
- r-efi offers `MIT OR Apache-2.0 OR LGPL-2.1-or-later`. Codehaven uses it under MIT or Apache-2.0.

To list every license yourself: `cargo metadata --format-version 1 --locked`, then read the
`license` field of each package.

## Bundled JavaScript

`crates/ch-web/static/htmx.min.js` and `htmx-sse.js` are htmx 2.0.4 (https://htmx.org), licensed
under the Zero-Clause BSD license (0BSD): permission to use, copy, modify and distribute with or
without fee, with no conditions. `app.js` and the CSS files are Codehaven's own.
