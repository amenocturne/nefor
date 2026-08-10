# Release guide

The workspace version in `[workspace.package]` in `Cargo.toml` is the release source of truth. `just version` prints the version. Stable tags are annotated `vX.Y.Z`; nightly is a rolling prerelease tag/release and is not a version source.

## Stable release procedure

1. Confirm the intended compatibility bump against the policy in `CLAUDE.md`: breaking pre-public changes bump the minor component; compatible additions and fixes bump the patch component.
2. Update `CHANGELOG.md`: move applicable Unreleased entries into a dated/tagged version section without rewriting older entries.
3. Change the workspace version in `Cargo.toml` and let Cargo update the lockfile where required.
4. Run the release confidence tier in [testing.md](testing.md). Confirm `just version` prints the intended version and inspect `git diff`.
5. Commit the release as one complete unit using repository history style.
6. Push the commit normally. The installed `.githooks/pre-push` first runs Clippy and `just test`. If no `v<workspace-version>` tag exists anywhere in local history, it creates an annotated tag at `HEAD` and pushes that tag itself.
7. Verify the tag points at the version-bump commit and watch the `release` GitHub Actions workflow. Do not create a second tag or move a published tag.

The pre-push hook makes a version bump operationally significant: pushing any commit while the committed workspace version has no corresponding local tag will cut that tag at the pushed `HEAD`. Keep the version unchanged during ordinary development and never bypass this behavior casually. A failed tag push leaves the local tag in place and reports that it must be pushed manually.

## What the workflow publishes

`.github/workflows/release.yml` runs for stable version tags, a daily 05:05 UTC schedule, and manual dispatch. It builds locked release workspaces for macOS ARM64, Linux x86_64, and Linux ARM64. Intel macOS is not currently produced.

`tools/bundle-release.sh` assembles each tarball with:

- `bin/nefor` and the standalone `bin/mag` compiler;
- every plugin binary discovered from Cargo package metadata under `plugins/`;
- `share/nefor/plugins.manifest` containing that exact discovered set;
- the shipped `starter`, license, README, and changelog.

The workflow uploads tarballs plus SHA-256 files, creates the GitHub release from the tag commit, and records the binary's build-script-derived version in the title. Stable releases then update `amenocturne/homebrew-tap`: the unversioned formula and a keg-only versioned formula are committed and pushed by the workflow. This final job requires `TAP_PUSH_TOKEN`. A GitHub release can therefore succeed while the tap update fails; inspect every job.

## Nightly behavior

Scheduled runs and manual `nightly` dispatch delete the prior nightly release and tag, then recreate them at the workflow commit as a prerelease. Nightlies do not update Homebrew formulas. Manual dispatch can also name a stable tag; `gh release create --target` can create that tag at the dispatched workflow commit, but the workflow does not validate the tag against the Cargo version. Prefer the tag-push path for stable releases.

## Release verification and recovery

Verify release assets contain all three target tarballs and checksum files, the binary version matches the intended tag, and the stable tap commit uses the new URLs and checksums. The bundle test establishes contents locally but does not exercise GitHub permissions or the external tap repository.

If build or publish fails, fix forward and re-run only while the tag still truthfully identifies the release commit. Do not move a tag after users may have fetched it. If the published artifact itself is wrong, make a new patch/minor release according to compatibility impact.
