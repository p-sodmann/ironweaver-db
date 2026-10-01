# Releasing

How a version of the Python package `ironweaver-db` is published to PyPI. The Rust crates are not published (`publish = false`) until 1.0 (step 17).

The workflow is `.github/workflows/release.yml`. A pushed tag `vX.Y.Z` builds abi3 wheels for Linux (manylinux x86_64, aarch64) and macOS (x86_64, arm64) plus the sdist, installs each wheel and runs the Python tests against it on Python 3.9 and 3.13, checks that the tag matches the package version, and publishes to PyPI with trusted publishing (no API token in the repository). Started by hand (`workflow_dispatch`), it builds and tests without publishing, or publishes to TestPyPI if `testpypi` is ticked.

## Once: set up trusted publishing

1. On PyPI (and, to rehearse, on TestPyPI), with the account that will own the project: *Your projects* → *Publishing* → *Add a new pending publisher* → GitHub: owner `p-sodmann`, repository `ironweaver-db`, workflow `release.yml`, environment `pypi` (for TestPyPI: `testpypi`), project name `ironweaver-db`.
2. On GitHub: *Settings* → *Environments*: create `pypi` and `testpypi`. Add yourself as a required reviewer of `pypi`, so that every upload waits for your approval.

## Publishing 0.1.0

1. Check that `main` is green: the `CI` workflow (Rust, the Python wheels on all four platforms, the short crash run, the MSRV and `cargo deny` checks) on the commit to release, and the latest `Nightly crash suite`.
2. The version is `version` in `[workspace.package]` of `Cargo.toml` (`0.1.0`); the wheel takes it from there. Move the `[Unreleased]` section of `CHANGELOG.md` under `## [0.1.0] - <date>`, commit (`release 0.1.0`), and push.
3. Optional rehearsal: run the *Release* workflow by hand with `testpypi` ticked, then `pip install -i https://test.pypi.org/simple/ ironweaver-db==0.1.0` in a fresh virtualenv and run a few lines of the README.
4. Tag and push: `git tag -a v0.1.0 -m "Ironweaver DB 0.1.0"` and `git push origin v0.1.0`.
5. Approve the `publish` job in the workflow run (the `pypi` environment). It uploads the four wheels and the sdist.
6. Check: `pip install ironweaver-db==0.1.0` in a fresh virtualenv on Linux and macOS, `python -c "import iwdb; print(iwdb.__version__)"`, then a GitHub release for the tag with the changelog section.
7. Mark M1's PyPI part done in `documentation/steps/README.md` and `step_7.md`.

A version can't be uploaded twice: if something goes wrong after the upload, fix it and release the next patch version.
