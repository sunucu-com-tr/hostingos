# Contributing to HostingOS

Thanks for your interest! This document covers building, testing and the
conventions the code base follows. The architecture rules are in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), and the web layer guide is in
[crates/hos-web/README.md](crates/hos-web/README.md).

## Licence, sign-off and CLA

HostingOS is licensed under the GNU Affero General Public License v3.0
only ([LICENSE](LICENSE)). By contributing you agree that your
contribution is published under that licence.

**Why a CLA will be required.** HostingOS is dual-licensed: besides the
AGPL, the maintainer offers it under a commercial licence to organisations
that cannot accept the AGPL's obligations ([COMMERCIAL.md](COMMERCIAL.md)),
and that income pays for development. A second licence can only be granted
for code the maintainer has the right to relicense. Outside contributions
will therefore need a Contributor License Agreement that grants this right;
you keep the copyright in your work, and your contribution stays available
under the AGPL.

The CLA text and the way to sign it will be published here; it is not
available yet.

**Until the CLA exists: DCO sign-off.** A sign-off covers the AGPL only,
not relicensing, so please open an issue before you work on a larger
change. Every commit in a pull request must carry a
`Signed-off-by` line, which certifies the
[Developer Certificate of Origin 1.1](https://developercertificate.org/)
(you wrote the change, or have the right to submit it under the project's
licence). `git commit -s` adds the line with the name and e-mail address
of your git configuration:

```
Signed-off-by: Jane Doe <jane@example.com>
```

Use your real name or the name you are known by publicly. Pull requests
with unsigned commits are not merged; `git rebase --signoff main` signs
existing commits of a branch.

## Building

You need:

- Rust: the version CI uses is in `.github/workflows/ci.yml`
  (`RUSTUP_TOOLCHAIN`); `rust-toolchain.toml` selects stable;
- PostgreSQL 16, local or hosted.

```sh
cp .env.example .env                 # DATABASE_URL, HOS_SECRET_KEY (openssl rand -base64 32), …
cargo run -p hos-web -- migrate
cargo run -p hos-web -- seed-demo    # demo company, catalogue, mock integrations
HOS_DEBUG=1 cargo run -p hos-web     # http://localhost:8080
```

`seed-demo` prints the logins it creates. Use a database of your own for
development, never a shared or production one.

A release binary and the container image are built the same way CI and
the Dockerfile do it:

```sh
cargo build --release --locked -p hos-web --bin hostingos   # target/release/hostingos
docker build -t hostingos .
```

The binary reads `static/` and `locales/` from disk (next to the binary,
the current directory, or `HOS_STATIC_DIR` / `HOS_LOCALES_DIR`).

## Testing

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- Integration tests use the database from `DATABASE_URL`. Each test creates
  its own tenant and removes it afterwards. Without a database the tests
  are skipped. Never point tests at a production database.
- Stop a running `hostingos serve` (or use `serve --no-worker`) while the
  tests run: its job worker would pick up the tests' jobs.
- A crate's integration tests form one test binary: add a module under
  `tests/it/` and a `mod` line in `tests/it/main.rs`. Do not add new files
  directly under `tests/`.
- CI runs the same commands against a fresh PostgreSQL, plus `cargo audit`,
  `cargo deny` (licence policy in `deny.toml`) and an end-to-end start of
  the demo image (`docker-compose.demo.yml`). After a merge to `main`, once
  all of that has passed, it publishes the `edge` image
  (docs/RELEASING.md, §8).

## Conventions

- **Edition 2024, no `unsafe`.** The workspace lints are in the root
  `Cargo.toml`, and `todo!` / `unimplemented!` / `dbg!` fail clippy.
- **Errors:** typed errors in libraries (`thiserror`) and `anyhow` only in
  the binary and CLI. Never `unwrap()` on input.
- **Database:** sqlx with runtime queries and tenant-scoped repository
  functions in `hos-db`. Every query is filtered by tenant.
- **Migrations are additive** (expand/contract, see docs/UPDATES.md §5):
  new tables, nullable columns and indexes. Destructive changes come in a
  later release. Never edit a migration that has been released.
- **Templates:** Askama, server-rendered. The CSP forbids inline scripts
  and styles, so use the existing component macros and CSS layers.
- **Translations:** every user-facing string goes in `locales/en/*.json`
  and `locales/tr/*.json` with identical keys (a unit test enforces this).
  Turkish text uses the correct letters (ç ğ ı İ ö ş ü).
- **Outbound HTTP** to endpoints configured in the admin panel goes through
  `hos_core::net` (outbound policy, no redirects to private addresses).
- **Dependencies:** only maintained crates, versions set once in
  `[workspace.dependencies]`, rustls (never OpenSSL). Explain new
  dependencies in the pull request.
- **Secrets** never go into the repository: no `.env` files, keys or
  credentials. Release signing keys stay with the maintainers
  (docs/RELEASING.md).

## Pull requests

- Keep each one focused. Describe what changed and why, and how you tested
  it.
- Add or update tests for behaviour changes.
- Add a line to the `Unreleased` section of `CHANGELOG.md` for anything
  users or operators notice.
- Report security problems privately (see [SECURITY.md](SECURITY.md)),
  not in a pull request or issue.
