# Security policy

## Reporting a vulnerability

Please **do not open a public issue** for security problems.

Report them privately with GitHub's **Report a vulnerability** button on
the repository's Security → Advisories page
(`https://github.com/sunucu-com-tr/hostingos/security/advisories/new`).


Please include:

- the affected version (`hostingos version`, or the version shown on the
  system status page), plus the install type (binary or container) and the
  configuration that matters;
- steps to reproduce, the impact, and a proof of concept if you have one.

We aim to acknowledge reports within a few working days. We will keep you
informed while we work on a fix and credit you in the release notes unless
you prefer otherwise.

## Supported versions

Until 1.0, only the latest release receives security fixes. Security
releases are marked as such in `CHANGELOG.md` and in the signed update
manifest. Installations with update checks enabled show them prominently
on the updates page (docs/UPDATES.md).

## Release integrity

Release archives, the update manifest and the checksum list are signed with
minisign. The trusted public keys are listed in
`crates/hos-update/keys/trusted.pub`. Release builds carry GitHub
build-provenance attestations. docs/RELEASING.md explains how to verify a
release by hand.

No signing key has been generated yet, so `trusted.pub` is empty and no
signed release exists. Until then, build from source and check the
commit you build from.

## Scope notes

- The `mock` payment, hypervisor, panel and registrar integrations are for
  development and demos only. They are refused unless debug mode,
  `HOS_ALLOW_MOCK_INTEGRATIONS` or demo mode (`HOS_DEMO_MODE`) is on.
  Enabling them in production is a configuration error, not a
  vulnerability.
- Demo mode (docs/DEMO.md) publishes fixed logins on purpose and lists
  the residual risks it accepts. A way around one of its blocks (outbound
  traffic, changing the demo logins, reaching other visitors' data beyond
  what docs/DEMO.md describes) is in scope; the published passwords are
  not.
- `HOS_OUTBOUND_POLICY` controls which addresses the integrations
  configured in the admin panel may reach (docs/INSTALL.md, section 4).
  Loopback, link-local and cloud metadata addresses are always refused; a
  way around that is in scope.
