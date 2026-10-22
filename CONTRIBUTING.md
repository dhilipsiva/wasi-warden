# Contributing

Thanks for helping. wasi-warden is security software: a change is judged first by what it lets an
untrusted plan or guest do. Read the README's Status and Security model sections, and
[docs/threat-model.md](docs/threat-model.md), before changing a capability.

## Setting up

```bash
nix develop        # the pinned toolchain: Rust with wasm32-wasip2, chromedriver, just
just ci            # the gate CI runs; `just --list` shows each recipe
```

`just ci` runs fmt, clippy with `-D warnings`, the unit tests, the docs, `cargo deny`, the WIT parse, the
component builds and the three end-to-end suites. It must pass before a pull request.

## How changes are made here

- **Tests first.** A fix or feature starts with a test that fails without it. For a security rule, also
  check that removing the guard makes some test fail: a rule no test notices is not pinned.
- **Both paths.** A rule that applies to the planner's JSON actions (`hostd/src/actions.rs`) usually also
  applies to the WIT capabilities (`hostd/src/capabilities.rs`). Put it in a shared module
  (`sandbox.rs`, `browser.rs`, `budget.rs`) and test it through both. The probe guest
  (`tests/probe-agent`) drives the WIT side.
- **New planner actions** go in `hostd/src/registry.rs`. It generates the catalog the planner sees and
  drives the executor's dispatch.
- **Secrets and the audit log.** Anything bound for the model goes through `build_chat_request`. A new
  WIT argument that carries data, rather than describing the call, belongs in the audit layer's
  `DATA_ARGUMENTS`.
- **Defaults deny.** A new capability is off until hostd.toml turns it on, and `hostd.toml.example`
  documents it.
- **Keep the README true.** A change to what works, or to a limit, updates the README's Status or
  Security model in the same commit.

## Commits and pull requests

Short imperative subjects (≤ 50 characters), then the why and the impact. A pull request says what it
changes in the policy, names any new capability, config key or default, and links the issue it closes.

## Reporting a vulnerability

Do not open a public issue for a security problem. Use GitHub's private vulnerability reporting on this
repository, where it is enabled, or contact the maintainer listed on the GitHub profile.

## Licence

By contributing, you agree that your contribution is licensed under MIT OR Apache-2.0, like the rest of
the repository.
