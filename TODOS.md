# TODOS — soma-observe

## Release / distribution pipeline (v1-release-blocker)

**What:** Build and publish the install artifacts the install doc promises.

**Why:** `docs/install-design.md` advertises `docker compose up`, `cargo install soma-observe`, and a `curl -fsSL .../install.sh | sh` pointing at `ghcr.io/chaitugsk07/soma-observe` and GitHub Releases. None of those exist until a pipeline builds and publishes them. Without it, the headline "one-step install" works only on the author's machine.

**Scope:**
- `Dockerfile` — static binary (musl target recommended to honor the single-binary promise).
- GitHub Actions workflow — build → test → publish image + multi-arch binaries on tag.
- Version scheme — semver (`v0.1.0`); `:latest` is an alias only, document pinning a version tag for production.
- `install.sh` released as a GitHub Release asset.

**Context:** Surfaced by `/plan-eng-review` (Step 0 distribution check + outside-voice #10). Lane D in the parallelization plan — independent of core service work, build once the binary ingests/queries. Not blocking for core development; blocking before announcing v1.

**Depends on / blocked by:** a runnable binary (so the image has something to package). Otherwise independent.
