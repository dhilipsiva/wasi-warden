# Releasing wasi-warden

This runbook goes with `.github/workflows/release.yml`, which was cut from the products repository's
template. Keep the two true together: a change to one changes the other in the same commit.

## What a release is

- **Channel:** a GitHub Release. Nothing goes to crates.io; hostd ships as binaries.
- **Assets:**
  - `hostd-X.Y.Z-x86_64-unknown-linux-gnu.tar.gz` and `hostd-X.Y.Z-aarch64-unknown-linux-gnu.tar.gz`.
    Each holds the binary, README.md, the licences and `hostd.toml.example`.
  - `agent_core.wasm`, the planner component.
  - `SHA256SUMS`.
  - A build-provenance attestation for each asset.
- **The version:** `hostd/Cargo.toml`'s. The tag must equal it.
- **Secrets:** none beyond the workflow's own token.

## Before tagging

1. CI is green on `main` for the commit you will tag, the `readme` job included.
2. Bump `version` in `hostd/Cargo.toml` (and `agent-core/Cargo.toml`, to keep them together). Commit the
   refreshed `Cargo.lock`.
3. Roll `CHANGELOG.md`: rename `[Unreleased]` to `[X.Y.Z] - YYYY-MM-DD`, and open a new, empty
   `[Unreleased]`. The release notes are that section; the preflight refuses a tag without it.
4. Rehearse: run the Release workflow by hand (Actions, Release, "Run workflow"). A manual run is always a
   dry run. It builds both binaries and the component, writes SHA256SUMS and renders the notes, and creates
   nothing.

## Cutting the release (the owner)

```sh
git switch main && git pull --ff-only
git tag -a vX.Y.Z -m "wasi-warden X.Y.Z"
git push origin vX.Y.Z
```

The workflow then runs: preflight, then CI, then the binaries and the component, then a draft GitHub
Release (checksums, provenance), then undraft. Nothing irreversible happens before the release is made
public.

## If it fails

- **Before `finalize`:** fix the cause on `main`, delete the tag locally and on the remote, delete the
  draft release, and tag again.
- **After `finalize`:** do not reuse the version. Release X.Y.Z+1.

## After the release

- Check that the release lists both archives, `agent_core.wasm` and `SHA256SUMS`, and that
  `sha256sum --check SHA256SUMS` passes on the downloads.
- In the release body, the launch note states what works, the known limits (the README's Status section)
  and how to report a vulnerability. Claim only what was measured.
