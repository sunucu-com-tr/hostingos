# Third-party notices

HostingOS is licensed under the GNU Affero General Public License v3.0 only
(see [LICENSE](LICENSE)). The repository, the Docker image and the release
archives contain, or are built from, the third-party works listed below,
each under its own licence. This file is a summary; where it differs from a
licence file, the licence file applies.

## Browser libraries (`static/vendor/`)

| Component | Version | Files | Licence | Upstream | Licence text in this repository |
|---|---|---|---|---|---|
| htmx | 2.0.4 (byte-identical to `dist/htmx.min.js` at tag `v2.0.4`) | `static/vendor/htmx.min.js` | 0BSD | https://github.com/bigskysoftware/htmx | `static/vendor/htmx.LICENSE.txt` (the `LICENSE` file at tag `v2.0.4`, unmodified; reproduced below) |
| noVNC, RFB client library (`core/` only, without noVNC's own UI) | 1.7.0 (`static/vendor/novnc/SOURCE.txt`) | `static/vendor/novnc/core/`, `AUTHORS` | MPL-2.0; `core/crypto/des.js` carries its own notices (see below) | https://github.com/novnc/noVNC | `static/vendor/novnc/LICENSE.txt`, `static/vendor/novnc/docs/LICENSE.MPL-2.0` (noVNC also ships `docs/LICENSE.BSD-2-Clause`, `docs/LICENSE.BSD-3-Clause`, `docs/LICENSE.OFL-1.1`) |
| pako, ES-module port shipped inside noVNC | based on pako 1.0.3 (`vendor/pako/README.md`) | `static/vendor/novnc/vendor/pako/` | MIT | https://github.com/nodeca/pako | `static/vendor/novnc/vendor/pako/LICENSE` |

noVNC and pako are vendored unchanged from the npm package
`@novnc/novnc@1.7.0` (`static/vendor/novnc/SOURCE.txt`). The noVNC files are
MPL-2.0: their source form is the files themselves, and changes to them stay
under the MPL-2.0. `static/js/console.js` (HostingOS code) loads the library
as an ES module.

`core/crypto/des.js` is not plain MPL-2.0: noVNC's `LICENSE.txt` lists it
under "various BSD style licenses". Its header keeps a permissive notice by
Widget Workshop, Inc. (1996) and a copyright line of AT&T Laboratories
Cambridge (1999) for the changes made for VNC.

### htmx licence (0BSD)

Copied from the `LICENSE` file of htmx at tag `v2.0.4`:

```
Zero-Clause BSD
=============

Permission to use, copy, modify, and/or distribute this software for
any purpose with or without fee is hereby granted.

THE SOFTWARE IS PROVIDED “AS IS” AND THE AUTHOR DISCLAIMS ALL
WARRANTIES WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES
OF MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE
FOR ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY
DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN
AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT
OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
```

## Fonts (`static/fonts/`)

All fonts are under the SIL Open Font License 1.1 and are used unmodified.
Each `OFL.txt` holds the copyright line of the font's authors and the licence
text. A `SOURCE.txt` in the same directory records the npm package the
files were copied from. Geist comes from Vercel's `geist` package; the
other fonts are Fontsource builds of the Google Fonts releases
(https://github.com/google/fonts), latin and latin-ext subsets of the
variable (weight axis) fonts.

| Font | Copied from | Files | Licence text | Project |
|---|---|---|---|---|
| Geist, Geist Mono | `geist@1.9.2` (fonts and `OFL.txt` byte-identical to the package's files; `static/fonts/SOURCE.txt`) | `static/fonts/Geist-Variable.woff2`, `static/fonts/GeistMono-Variable.woff2` | `static/fonts/OFL.txt` | https://github.com/vercel/geist-font |
| DM Sans | `@fontsource-variable/dm-sans@5.3.0` | `static/fonts/dm-sans/` | `static/fonts/dm-sans/OFL.txt` | https://github.com/googlefonts/dm-fonts |
| Figtree | `@fontsource-variable/figtree@5.3.0` | `static/fonts/figtree/` | `static/fonts/figtree/OFL.txt` | https://github.com/erikdkennedy/figtree |
| Fraunces | `@fontsource-variable/fraunces@5.3.0` | `static/fonts/fraunces/` | `static/fonts/fraunces/OFL.txt` | https://github.com/undercasetype/Fraunces |
| IBM Plex Sans | `@fontsource-variable/ibm-plex-sans@5.3.0` | `static/fonts/ibm-plex-sans/` | `static/fonts/ibm-plex-sans/OFL.txt` | https://github.com/IBM/plex |
| Inter | `@fontsource-variable/inter@5.3.0` | `static/fonts/inter/` | `static/fonts/inter/OFL.txt` | https://github.com/rsms/inter |
| Manrope | `@fontsource-variable/manrope@5.3.0` | `static/fonts/manrope/` | `static/fonts/manrope/OFL.txt` | https://github.com/sharanda/manrope |
| Outfit | `@fontsource-variable/outfit@5.3.0` | `static/fonts/outfit/` | `static/fonts/outfit/OFL.txt` | https://github.com/Outfitio/Outfit-Fonts |
| Plus Jakarta Sans | `@fontsource-variable/plus-jakarta-sans@5.3.0` | `static/fonts/plus-jakarta-sans/` | `static/fonts/plus-jakarta-sans/OFL.txt` | https://github.com/tokotype/PlusJakartaSans |
| Source Serif 4 | `@fontsource-variable/source-serif-4@5.3.1` | `static/fonts/source-serif-4/` | `static/fonts/source-serif-4/OFL.txt`, taken from Google Fonts (`ofl/sourceserif4/OFL.txt`) because the npm package's licence file names only "Google Inc." | https://github.com/adobe-fonts/source-serif |
| Space Grotesk | `@fontsource-variable/space-grotesk@5.3.0` | `static/fonts/space-grotesk/` | `static/fonts/space-grotesk/OFL.txt` | https://github.com/floriankarsten/space-grotesk |

Two font files are also compiled into the `hostingos` binary for invoice
PDFs (`crates/hos-pdf/src/font.rs`): `Geist-Variable.woff2` and Inter's
`inter-latin-ext-wght.woff2`. A generated PDF embeds only the glyphs it
uses; the OFL does not apply to documents made with the fonts.

The icon sprite (`static/img/icons.svg`), logos, favicon, theme previews,
`assets/icon.svg` and all CSS and JavaScript under `static/css/` and
`static/js/` are part of HostingOS (AGPL-3.0-only).

## Open points

- `static/vendor/novnc/core/crypto/des.js`: the AT&T Laboratories
  Cambridge (1999) copyright line in its header comes without a licence
  grant of its own, and noVNC's `LICENSE.txt` lists the file under "various
  BSD style licenses" without naming one. The file is used as noVNC ships
  it.
- The published `selectors` crate contains no licence file (see the Rust
  crates section).
- The release archives and the image do not yet contain the full licence
  text and copyright notices of every Rust crate listed in the next
  section; until they do, those are in each crate's source package on
  crates.io.

## Rust crates compiled into `hostingos`

The binary statically links the crates listed below. All of them come from
crates.io. The list was generated with

```sh
cargo tree --locked -p hos-web -e normal,build --target all --prefix none --format '{p}|{l}'
```

for the `Cargo.lock` of this tree. It covers every target platform, so it
includes crates that only Windows, macOS, Android or WebAssembly builds use
and that the Linux release binaries do not contain. Test-only crates are
left out. Licences are as declared by each crate.

Where a crate offers a choice (`A OR B`), HostingOS uses it under a
permissive alternative (MIT or Apache-2.0, or another permissive one on the
list), never under a copyleft alternative. Each crate's licence text and
copyright notices are in its source package on crates.io; every release also
ships a CycloneDX SBOM listing the crates with their licences. The licence
policy is enforced in CI with `cargo deny` ([deny.toml](deny.toml)).

The MPL-2.0 crates (`cssparser`, `cssparser-macros`, `selectors`,
`dtoa-short`) are used unmodified; their source code is available from
crates.io. (The published `selectors` package contains no licence file; its
licence is declared in its `Cargo.toml`. The MPL-2.0 text is at
https://mozilla.org/MPL/2.0/ and in `static/vendor/novnc/docs/LICENSE.MPL-2.0`.)

### Notices carried by crates

`moka` 0.12.16 ships a `NOTICE` file. In summary: most of Moka is
dual-licensed MIT / Apache-2.0, but `src/common/frequency_sketch.rs` and
`src/common/timer_wheel.rs` were ported from the Java Caffeine library and
are licensed solely under the Apache License 2.0.

`subsetter` 0.2.6 ships a `NOTICE` file stating that parts of its code were
adapted from ttf-parser and from fonttools, both under the MIT licence:

```
ttf-parser: Copyright (c) 2018 Yevhenii Reizner
fonttools:  Copyright (c) 2017 Just van Rossum

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.
```

(The rest of subsetter's `NOTICE` concerns font files in its test suite,
which are not part of the compiled crate.)

### Crates by licence

489 crates (counted per version), grouped by the licence expression they declare. Legacy `/` separators are written as `OR`.

**MIT OR Apache-2.0** (283)

aead 0.6.1, aes 0.9.4, allocator-api2 0.2.21, android_system_properties 0.1.6, anstream 1.0.0, anstyle 1.0.14, anstyle-parse 1.0.0, anstyle-query 1.1.5, anstyle-wincon 3.0.11, anyhow 1.0.104, arc-swap 1.9.5, argon2 0.6.0, arrayvec 0.7.8, askama 0.16.1, askama_derive 0.16.1, askama_macros 0.16.1, askama_parser 0.16.1, async-compression 0.4.52, async-trait 0.1.92, base32 0.5.1, base64 0.22.1, base64 0.23.1, basic-toml 0.1.10, bitflags 2.13.2, blake2 0.11.0, block-buffer 0.10.4, block-buffer 0.12.1, blowfish 0.10.0, btoi 0.5.0, bumpalo 3.20.4, cc 1.11.2, cfg-if 1.0.5, chacha20 0.10.2, chrono 0.4.45, chrono-tz 0.10.4, cipher 0.4.4, cipher 0.5.2, clap 4.6.7, clap_builder 4.6.7, clap_derive 4.6.7, clap_lex 1.1.1, cmake 0.1.58, colorchoice 1.0.5, compression-codecs 0.4.47, compression-core 0.4.34, core-foundation 0.9.4, core-foundation 0.10.1, core-foundation-sys 0.8.7, core_detect 1.0.0, cpubits 0.1.1, cpufeatures 0.2.17, cpufeatures 0.3.1, crc 3.4.0, crc-catalog 2.5.0, crc32fast 1.5.5, critical-section 1.2.0, cron 0.17.0, crossbeam-channel 0.5.17, crossbeam-epoch 0.9.21, crossbeam-queue 0.3.14, crossbeam-utils 0.8.23, crypto-common 0.1.7, crypto-common 0.2.2, ctr 0.10.1, des 0.8.1, digest 0.10.7, digest 0.11.3, displaydoc 0.2.7, dtoa 1.0.11, either 1.19.0, email-encoding 0.4.3, errno 0.3.15, etcetera 0.8.0, fdeflate 0.3.7, filetime 0.2.30, find-msvc-tools 0.1.15, flate2 1.1.11, font-types 0.11.3, form_urlencoded 1.2.2, futures 0.3.34, futures-channel 0.3.34, futures-core 0.3.34, futures-executor 0.3.34, futures-intrusive 0.5.0, futures-io 0.3.34, futures-macro 0.3.34, futures-sink 0.3.34, futures-task 0.3.34, futures-util 0.3.34, getrandom 0.2.17, getrandom 0.3.4, getrandom 0.4.3, gif 0.14.2, glob 0.3.4, hashbrown 0.15.5, hashbrown 0.17.1, hashlink 0.10.0, heck 0.5.0, hex 0.4.3, hickory-net 0.26.3, hickory-proto 0.26.3, hickory-resolver 0.26.3, hkdf 0.12.4, hmac 0.12.1, hmac 0.13.0, home 0.5.12, html5ever 0.40.1, http 1.5.0, httparse 1.10.1, httpdate 1.0.3, hybrid-array 0.4.15, iana-time-zone 0.1.65, iana-time-zone-haiku 0.1.2, ident_case 1.0.1, idna 1.1.0, image-webp 0.2.4, inout 0.1.4, inout 0.2.6, ipconfig 0.3.4, ipnet 2.12.2, is_terminal_polyfill 1.70.2, itoa 1.0.19, jni 0.22.4, jni-macros 0.22.4, jni-sys 0.4.1, jni-sys-macros 0.4.1, jobserver 0.1.35, js-sys 0.3.107, krilla 0.8.2, lazy_static 1.5.1, libc 0.2.191, lock_api 0.4.14, log 0.4.35, manyhow 0.11.4, manyhow-macros 0.11.4, markup5ever 0.40.0, md-5 0.10.6, md-5 0.11.0, mime 0.3.18, mysql-common-derive 0.32.2, mysql_async 0.37.1, mysql_common 0.37.3, ndk-context 0.1.1, num-bigint 0.4.8, num-integer 0.1.47, num-traits 0.2.19, once_cell 1.21.4, once_cell_polyfill 1.70.2, openssl-probe 0.2.2, parking_lot 0.12.5, parking_lot_core 0.9.12, password-hash 0.6.1, pdf-writer 0.15.0, percent-encoding 2.3.2, pkg-config 0.3.34, plain 0.2.3, png 0.18.1, polycool 0.4.0, ppv-lite86 0.2.21, prefix-trie 0.8.4, proc-macro-crate 3.5.0, proc-macro-utils 0.10.0, proc-macro2 1.0.107, psl 2.1.255, psl-types 2.0.11, qrcode 0.14.1, quick-error 2.0.1, quote 1.0.47, rand 0.8.8, rand 0.9.5, rand 0.10.4, rand_chacha 0.3.1, rand_chacha 0.9.0, rand_core 0.6.4, rand_core 0.9.5, rand_core 0.10.1, read-fonts 0.39.2, regex 1.13.1, regex-automata 0.4.18, regex-syntax 0.8.11, reqwest 0.13.5, resolv-conf 0.7.6, rustc_version 0.4.1, rustls-pki-types 1.16.0, rustls-platform-verifier 0.7.1, rustls-platform-verifier-android 0.2.0, rustversion 1.0.23, scopeguard 1.2.0, security-framework 3.7.0, security-framework-sys 2.17.0, semver 1.0.28, serde 1.0.229, serde_core 1.0.229, serde_derive 1.0.229, serde_json 1.0.154, serde_path_to_error 0.1.20, serde_urlencoded 0.7.1, servo_arc 0.5.0, sha1 0.10.7, sha1 0.11.0, sha2 0.10.9, sha2 0.11.1, shlex 2.0.1, signal-hook-registry 1.4.8, simdutf8 0.1.5, siphasher 1.0.4, skrifa 0.42.1, smallvec 1.16.5, socket2 0.6.5, sqlx 0.8.6, sqlx-core 0.8.6, sqlx-macros 0.8.6, sqlx-macros-core 0.8.6, sqlx-postgres 0.8.6, stable_deref_trait 1.2.1, string_cache 0.11.0, string_cache_codegen 0.11.2, stringprep 0.1.5, subsetter 0.2.6, syn 2.0.119, syn 3.0.10, system-configuration 0.7.0, system-configuration-sys 0.6.0, tagptr 0.2.0, tar 0.4.46, tendril 0.5.1, thiserror 2.0.21, thiserror-impl 2.0.21, thread_local 1.1.10, tokio-rustls 0.26.6, toml_datetime 1.1.3+spec-1.1.0, toml_edit 0.25.19+spec-1.1.0, toml_parser 1.1.5+spec-1.1.0, ttf-parser 0.25.1, tungstenite 0.29.0, typenum 1.20.1, unicase 2.10.0, unicode-bidi 0.3.18, unicode-bidi-mirroring 0.4.0, unicode-ccc 0.4.0, unicode-script 0.5.9, unicode-segmentation 1.13.3, universal-hash 0.6.1, url 2.5.8, version_check 0.9.5, wasm-bindgen 0.2.130, wasm-bindgen-futures 0.4.80, wasm-bindgen-macro 0.2.130, wasm-bindgen-macro-support 0.2.130, wasm-bindgen-shared 0.2.130, web-sys 0.3.107, web_atoms 0.3.0, weezl 0.1.12, widestring 1.2.1, windows-core 0.62.2, windows-implement 0.60.2, windows-interface 0.59.3, windows-link 0.2.1, windows-registry 0.6.1, windows-result 0.4.1, windows-strings 0.5.1, windows-sys 0.48.0, windows-sys 0.52.0, windows-sys 0.61.2, windows-targets 0.48.5, windows-targets 0.52.6, windows_aarch64_gnullvm 0.48.5, windows_aarch64_gnullvm 0.52.6, windows_aarch64_msvc 0.48.5, windows_aarch64_msvc 0.52.6, windows_i686_gnu 0.48.5, windows_i686_gnu 0.52.6, windows_i686_gnullvm 0.52.6, windows_i686_msvc 0.48.5, windows_i686_msvc 0.52.6, windows_x86_64_gnu 0.48.5, windows_x86_64_gnu 0.52.6, windows_x86_64_gnullvm 0.48.5, windows_x86_64_gnullvm 0.52.6, windows_x86_64_msvc 0.48.5, windows_x86_64_msvc 0.52.6, write-fonts 0.48.1, xmp-writer 0.3.3

**MIT** (93)

atoi 2.0.0, axum 0.8.9, axum-core 0.5.6, axum-macros 0.5.1, bcrypt 0.19.3, bytes 1.12.1, color_quant 1.1.0, combine 4.6.9, core_maths 0.1.1, darling 0.23.0, darling_core 0.23.0, darling_macro 0.23.0, data-encoding 2.11.1, derive_more 2.1.1, derive_more-impl 2.1.1, dotenvy 0.15.7, email_address 0.2.9, float-cmp 0.9.0, fs_extra 1.3.0, generic-array 0.14.7, h2 0.4.20, hostname 0.4.2, http-body 1.1.0, http-body-util 0.1.5, http-range-header 0.4.2, hyper 1.12.0, hyper-util 0.1.21, imagesize 0.14.0, keyed_priority_queue 0.4.2, lettre 0.11.23, libm 0.2.16, libredox 0.1.25, lru 0.18.5, matchers 0.2.0, mime_guess 2.0.5, minisign-verify 0.3.0, mio 1.2.5, multer 3.1.0, new_debug_unreachable 1.0.6, nom 8.0.0, nu-ansi-term 0.50.3, phf 0.11.3, phf 0.12.1, phf 0.14.0, phf_codegen 0.14.0, phf_generator 0.11.3, phf_generator 0.14.0, phf_macros 0.11.3, phf_macros 0.14.0, phf_shared 0.11.3, phf_shared 0.12.1, phf_shared 0.14.0, precomputed-hash 0.1.1, pulldown-cmark 0.13.6, pulldown-cmark-escape 0.11.0, quick-xml 0.42.0, redox_syscall 0.5.18, redox_syscall 0.9.5, rust_decimal 1.43.0, rustybuzz 0.20.1, saturating 0.1.0, schannel 0.1.30, sharded-slab 0.1.7, simd-adler32 0.3.10, slab 0.4.12, spin 0.9.9, strict-num 0.1.1, strsim 0.11.1, synstructure 0.14.0, tokio 1.54.1, tokio-macros 2.7.2, tokio-stream 0.1.19, tokio-tungstenite 0.29.0, tokio-util 0.7.20, totp-rs 6.0.0, tower 0.5.3, tower-http 0.6.11, tower-http 0.7.1, tower-layer 0.3.3, tower-service 0.3.3, tracing 0.1.44, tracing-attributes 0.1.31, tracing-core 0.1.36, tracing-log 0.2.0, tracing-serde 0.2.0, tracing-subscriber 0.3.23, try-lock 0.2.5, twox-hash 2.1.5, want 0.3.2, winnow 0.7.15, winnow 1.0.5, wuff 0.2.10, zmij 1.0.23

**Apache-2.0 OR MIT** (31)

aes-gcm 0.11.1, atomic-waker 1.1.2, autocfg 1.5.1, base64ct 1.8.3, cmov 0.5.4, const-oid 0.10.3, ctutils 0.4.8, encodify 1.0.2, equivalent 1.0.3, event-listener 5.4.2, fastrand 2.5.0, fnv 1.0.7, ghash 0.6.0, hashify 0.3.2, idna_adapter 1.2.2, indexmap 2.14.2, kurbo 0.13.1, mail-parser 1.0.1, multiversion_no_op 1.0.0, parking 2.2.1, phc 0.6.1, pin-project-lite 0.2.17, polyval 0.7.3, portable-atomic 1.15.0, rustc-hash 2.1.3, simd_cesu8 1.2.0, utf8_iter 1.1.0, utf8parse 0.2.2, utf_types 1.0.0, uuid 1.29.2, zeroize 1.9.1

**Unicode-3.0** (18)

icu_collections 2.4.0, icu_locale_core 2.4.0, icu_normalizer 2.4.0, icu_normalizer_data 2.4.0, icu_properties 2.4.0, icu_properties_data 2.4.0, icu_provider 2.4.0, litemap 0.8.3, potential_utf 0.1.6, tinystr 0.8.5, writeable 0.6.4, yoke 0.8.4, yoke-derive 0.8.4, zerofrom 0.1.8, zerofrom-derive 0.1.8, zerotrie 0.2.5, zerovec 0.11.9, zerovec-derive 0.11.7

**Unlicense OR MIT** (8)

aho-corasick 1.1.5, byteorder 1.5.0, byteorder-lite 0.1.0, memchr 2.8.3, same-file 1.0.6, termcolor 1.4.1, walkdir 2.5.0, winapi-util 0.1.11

**BSD-3-Clause** (4)

alloc-no-stdlib 3.0.0, alloc-stdlib 0.3.0, subtle 2.6.1, tiny-skia-path 0.12.0

**ISC** (4)

ego-tree 0.11.1, rustls-webpki 0.103.17, scraper 0.29.0, untrusted 0.9.0

**MPL-2.0** (4)

cssparser 0.38.0, cssparser-macros 0.7.1, dtoa-short 0.3.5, selectors 0.41.0

**(MIT OR Apache-2.0) AND Unicode-3.0** (3)

unicode-ident 1.0.27, unicode-normalization 0.1.26, unicode-properties 0.1.5

**Apache-2.0** (3)

rpassword 7.5.5, rtoolbox 0.0.6, sync_wrapper 1.0.2

**Apache-2.0 OR ISC OR MIT** (3)

hyper-rustls 0.27.10, rustls 0.23.46, rustls-native-certs 0.8.5

**Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT** (3)

wasi 0.11.1+wasi-snapshot-preview1, wasip2 1.0.4+wasi-0.2.12, wit-bindgen 0.57.1

**CDLA-Permissive-2.0** (3)

webpki-root-certs 1.0.12, webpki-roots 0.26.11, webpki-roots 1.0.12

**Zlib OR Apache-2.0 OR MIT** (3)

bytemuck 1.25.2, bytemuck_derive 1.12.1, tinyvec 1.13.3

**Apache-2.0 OR BSL-1.0 OR MIT** (2)

wasite 0.1.0, whoami 1.6.1

**BSD-2-Clause OR Apache-2.0 OR MIT** (2)

zerocopy 0.8.63, zerocopy-derive 0.8.63

**CC0-1.0 OR MIT-0 OR Apache-2.0** (2)

constant_time_eq 0.4.2, dunce 1.0.5

**MIT OR Apache-2.0 OR LGPL-2.1-or-later** (2)

r-efi 5.3.0, r-efi 6.0.0

**MIT OR Apache-2.0 OR Zlib** (2)

zune-core 0.5.3, zune-jpeg 0.5.15

**MIT OR Zlib OR Apache-2.0** (2)

miniz_oxide 0.8.9, miniz_oxide 0.9.1

**Zlib** (2)

foldhash 0.1.5, foldhash 0.2.0

**(Apache-2.0 OR MIT) AND BSD-3-Clause** (1)

encoding_rs 0.8.44

**(MIT OR Apache-2.0) AND Apache-2.0** (1)

moka 0.12.16

**0BSD** (1)

quoted_printable 0.5.2

**0BSD OR MIT OR Apache-2.0** (1)

adler2 2.0.1

**Apache-2.0 AND ISC** (1)

ring 0.17.14

**Apache-2.0 OR BSL-1.0** (1)

ryu 1.0.23

**BSD-2-Clause** (1)

arrayref 0.3.9

**BSD-3-Clause AND MIT** (1)

brotli 9.1.3

**BSD-3-Clause OR MIT** (1)

brotli-decompressor 6.3.2

**ISC AND (Apache-2.0 OR ISC)** (1)

aws-lc-rs 1.19.0

**ISC AND (Apache-2.0 OR ISC) AND Apache-2.0 AND MIT AND BSD-3-Clause AND (Apache-2.0 OR ISC OR MIT) AND (Apache-2.0 OR ISC OR MIT-0)** (1)

aws-lc-sys 0.47.0

**MIT AND BSD-3-Clause** (1)

matchit 0.8.4
