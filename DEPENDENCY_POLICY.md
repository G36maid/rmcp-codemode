# Dependency Policy

This document describes how the `rmcp-codemode` crates select, update, and maintain
their dependencies.

## Goals

- Keep dependencies current and secure without churn that destabilizes the public API.
- Minimize the dependency footprint we impose on downstream users.
- Make dependency updates reviewable and traceable.

## Choosing dependencies

Before adding a new dependency we consider:

- **Necessity** — whether the functionality is worth an added dependency, or is small
  enough to implement directly. BM25 search, for example, is implemented in-tree
  rather than pulling in a search crate.
- **Maintenance & trust** — active maintenance, a healthy release history, and a
  permissive, compatible license (the workspace is `Apache-2.0`).
- **Footprint** — transitive dependency count and compile-time cost.
- **Optionality** — anything not needed by every user should sit behind a Cargo
  feature so downstream builds stay lean. The JS sandbox backend
  (`rmcp-codemode-quickjs`) is a separate crate for exactly this reason.

Version requirements are specified with caret (`^`, the Cargo default) constraints so
compatible upgrades are picked up automatically.

## Relationship to the RMCP SDK dependency policy

This crate intentionally depends on `rmcp` (crates.io, stable releases). It exists
outside the SDK's workspace precisely so it can carry dependencies the SDK's policy
rules out — QuickJS via `rquickjs` chief among them (see
[modelcontextprotocol/rust-sdk#1437](https://github.com/modelcontextprotocol/rust-sdk/issues/1437)).

## Security updates

- Security fixes take priority over routine updates and are expedited.
- Advisories affecting our dependencies (e.g. via the
  [RustSec Advisory Database](https://rustsec.org/)) are addressed as soon as a fixed
  version is available; if a released version of the crate is impacted, a patched
  release is published and, where warranted, the affected versions are yanked from
  crates.io.

## Minimum Supported Rust Version

Dependency upgrades must not raise the workspace's declared MSRV (`rust-version` in
`Cargo.toml`) without an explicit, documented MSRV bump.
