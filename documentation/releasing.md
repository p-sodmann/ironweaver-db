# Releasing

How a version of Ironweaver DB is released. **A release is a git tag `vX.Y.Z` and a GitHub Release with its files, nothing else**: no crates.io, PyPI or image registry ([step 17](steps/step_17.md)'s Plan change). What a version number promises is [ADR 0060](adr/0060-versioning-policy.md), the versioning policy.

The workflow is `.github/workflows/release.yml`. A pushed tag `v*`:

1. **Gate**, before anything is built (`scripts/release-gate.sh`): the version is semver (pre-releases only `-alpha.N`, `-beta.N`, `-rc.N`), the tag is the workspace version and `Cargo.lock`'s, the release fixture `crates/iwdb/tests/fixtures/releases/<version>/` exists, and `CHANGELOG.md` has a `## [<version>]` section that isn't empty. A failed check fails the run with its reason, and nothing is built.
2. **Build and test**, from the tagged tree with no cache:
   - the Python wheels for five targets and the sdist; each wheel's tests run on Python 3.9 and 3.13, and the sdist builds a wheel;
   - the `iwdb-server` and `iwctl` binaries for the same five targets, archived, unpacked again and run with `--version`;
   - the image for linux/amd64 and linux/arm64, each built on its own architecture with buildx, run through CI's smoke test (`docker/smoke.sh`), saved with `docker save`, and loaded back.
3. **Assemble**: every file is there for this version, `SHA256SUMS` over them, and the notes: the CHANGELOG section, with a line on how to install.
4. **Publish**, the only job that can write (`contents: write`): a GitHub Release for the tag, created as a draft with its files and then published; marked a **pre-release** when the version has a suffix.

Started by hand (*Actions* → *Release* → *Run workflow*, or `gh workflow run release.yml --ref <branch>`), it is a **rehearsal**: everything but publish runs, for the workspace version; the gate only warns, the notes are the `[Unreleased]` section, and the files are the run's `release` artifact.

## The files of a release

| File | What it is |
|---|---|
| `ironweaver_db-<py>-cp39-abi3-<platform>.whl` (5) | **The Python package `ironweaver-db`**: one wheel holds both the embedded store (`iwdb.Store.open`) and the remote client (`iwdb.connect`). Linux x86_64 and aarch64 (manylinux, glibc 2.17 or later), macOS x86_64 and arm64, Windows x86_64; CPython 3.9 and later. `<py>` is the version in Python's spelling: `1.0.0`, or `1.0.0rc1` for `1.0.0-rc.1`. |
| `ironweaver_db-<py>.tar.gz` | The sdist: builds the wheel from source (Rust 1.99 or later). |
| `ironweaver-db-<version>-<target>.tar.gz` (4), `...-x86_64-pc-windows-msvc.zip` | `iwdb-server` (features gRPC, REST, Postgres projections, console, traces, as in the image) and `iwctl`, with the licenses, the README and the CHANGELOG. Targets: `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu` (glibc 2.28 or later: RHEL 8, Debian 10, Ubuntu 20.04 and newer), `x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc`. |
| `ironweaver-db-<version>-docker-linux-amd64.tar.gz`, `...-linux-arm64.tar.gz` | The image ([ADR 0034](adr/0034-docker-image-and-server-features.md)), one file per architecture: `docker load -i <file>` gives `iwdb-server:<version>`. |
| `SHA256SUMS` | The SHA-256 of every other file: `sha256sum -c SHA256SUMS` (macOS: `shasum -a 256 -c SHA256SUMS`). |

The binaries aren't signed. On macOS, a file downloaded with a browser is quarantined: `xattr -d com.apple.quarantine iwdb-server iwctl` (files downloaded with `curl` or `gh` aren't).

## Once: protect the tags (recommended)

A tag publishes, and a published tag is never moved (below). On GitHub, *Settings* → *Rules* → *Rulesets* → *New tag ruleset*: target `v*`, bypass list the owner only, rules *Restrict creations*, *Restrict updates* and *Restrict deletions*. Nothing else needs setting up: the workflow uses only `GITHUB_TOKEN`.

## A release, step by step

`X.Y.Z` is the version below. Since 2026-10-04 changes reach `main` through pull requests, so the release commit is a PR too.

1. **`main` is green**: the *CI* workflow on its head, and the latest *Nightly crash suite*.

   ```sh
   gh run list --workflow ci.yml --branch main --limit 1
   gh run list --workflow nightly.yml --limit 1
   ```

2. **The upstream check** ([upstream-check.md](steps/upstream-check.md)). Every `ironweaver-core` issue that is still open goes into the release's notes: a `### Known issues` subsection of the version's CHANGELOG section (step 3), with its workaround and link.

3. **The release commit**, on a branch from `main`:

   ```sh
   git switch -c release-X.Y.Z origin/main
   # Cargo.toml, [workspace.package]: version = "X.Y.Z"
   cargo build --workspace                       # updates Cargo.lock
   # CHANGELOG.md: rename "## [Unreleased]" to "## [X.Y.Z] - <today>", put a new
   # empty "## [Unreleased]" above it, add "### Known issues" if step 2 found any
   cargo test -p iwdb --test release_fixture -- --ignored generate_fixture
   cargo test -p iwdb --test release_fixture     # the new fixture opens
   sh scripts/release-gate.sh vX.Y.Z             # all checks pass
   git grep -n "<the old version>" -- '*.md'     # examples that print the version
   git add Cargo.toml Cargo.lock CHANGELOG.md crates/iwdb/tests/fixtures/releases/X.Y.Z && git commit -m "release X.Y.Z"
   git push -u origin release-X.Y.Z && gh pr create --title "Release X.Y.Z" --fill
   ```

   The fixture is written by this commit's code, so commit nothing else after writing it. Merge the PR when CI is green, with nothing else merged in between.

4. **Rehearse on the release commit** (optional, about an hour): `gh workflow run release.yml --ref main`. The gate now passes without warnings. Check the run's `release` artifact as in [After the release](#after-the-release).

5. **Tag and push**, from `main`'s head, which is the merged release commit:

   ```sh
   git switch main && git pull --ff-only
   git log -1 --format=%s                        # the release commit
   git tag -a vX.Y.Z -m "Ironweaver DB X.Y.Z"
   git push origin vX.Y.Z
   gh run watch "$(gh run list --workflow release.yml --limit 1 --json databaseId -q '.[0].databaseId')"
   ```

6. **After the release**: the checks below. Then step files, the README's status, and anything else that names the version.

## Pre-releases, and the rehearsal tag for 1.0

A pre-release (`X.Y.Z-rc.N`, `-alpha.N`, `-beta.N`; ADR 0060) goes through the same workflow, and its GitHub Release is marked **pre-release**. Its version bump, CHANGELOG section and fixture stay **off `main`**: it is tagged on a branch `release/X.Y.Z-rc.N` that is never merged, because a pre-release promises nothing to later versions. Steps 1 and 2 above still apply.

The rehearsal for 1.0 (step 17e's acceptance criterion), `v1.0.0-rc.1`:

```sh
git fetch origin && git switch -c release/1.0.0-rc.1 origin/main
sed -i.bak 's/^version = "0.1.0"$/version = "1.0.0-rc.1"/' Cargo.toml && rm Cargo.toml.bak
cargo build --workspace                                         # Cargo.lock
sed -i.bak 's/^## \[Unreleased\]$/## [1.0.0-rc.1] - '"$(date +%F)"'/' CHANGELOG.md && rm CHANGELOG.md.bak
cargo test -p iwdb --test release_fixture -- --ignored generate_fixture
cargo test -p iwdb --test release_fixture
sh scripts/release-gate.sh v1.0.0-rc.1                          # version=1.0.0-rc.1, python_version=1.0.0rc1, prerelease=true
git add Cargo.toml Cargo.lock CHANGELOG.md crates/iwdb/tests/fixtures/releases/1.0.0-rc.1
git commit -m "release 1.0.0-rc.1 (pre-release; not merged into main)"
git push -u origin release/1.0.0-rc.1
git tag -a v1.0.0-rc.1 -m "Ironweaver DB 1.0.0-rc.1 (pre-release)"
git push origin v1.0.0-rc.1
```

Then check the release as below, on a clean machine (one without a Rust toolchain or this repository) for the wheel and the image.

## After the release

On the release page (`gh release view vX.Y.Z -R p-sodmann/ironweaver-db --web`):

- the title is `Ironweaver DB X.Y.Z`, the notes are the CHANGELOG section (with its known issues) and the install line; a pre-release is marked **Pre-release** and isn't *Latest*;
- **14 files**: 5 wheels, the sdist, 5 binary archives, 2 image files, `SHA256SUMS`.

Then, in an empty directory:

```sh
V=X.Y.Z                                          # e.g. 1.0.0-rc.1
gh release download "v$V" -R p-sodmann/ironweaver-db
sha256sum -c SHA256SUMS                          # macOS: shasum -a 256 -c SHA256SUMS

# The wheel: pip picks the one for this platform from the downloaded files,
# or give it one wheel's URL from the release page
python3 -m venv venv
venv/bin/pip install --no-index --find-links . ironweaver-db
venv/bin/python -c 'import iwdb; print(iwdb.__version__)'          # $V
venv/bin/python -c '
import iwdb
with iwdb.Store.open("data") as store:
    with store.transaction() as tx:
        tx.upsert_node("alice", labels=["Person"], attr={"name": "Alice"})
    print(store.node("alice"))'

# The image (use the arm64 file on an arm64 machine)
docker load -i "ironweaver-db-$V-docker-linux-amd64.tar.gz"
docker run --rm "iwdb-server:$V" --version                         # iwdb-server $V (grpc, rest, postgres, console, otel)
docker run -d --name iwdb-check -p 127.0.0.1:7600:7600 -e IWDB_TLS_ENABLED=false \
  -e IWDB_SERVER_PLAINTEXT_PUBLIC=true -e IWDB_AUTH_BOOTSTRAP_PASSWORD=check-password "iwdb-server:$V"
sleep 5 && curl -fsS http://127.0.0.1:7600/v1/health/ready
venv/bin/python -c '
import iwdb
with iwdb.connect("http://127.0.0.1:7600", user="admin", password="check-password") as store:
    print(store.namespaces())'
docker rm -f iwdb-check

# The binaries of this platform (on Windows, unzip the .zip)
T=x86_64-unknown-linux-gnu                       # or aarch64-unknown-linux-gnu, x86_64-apple-darwin, aarch64-apple-darwin
tar -xzf "ironweaver-db-$V-$T.tar.gz"
"ironweaver-db-$V-$T/iwdb-server" --version
"ironweaver-db-$V-$T/iwctl" --version
```

(Plaintext on localhost is only for this check; [operations.md](operations.md#install) runs the image with TLS.)

## When something fails

**A pushed tag is never moved, deleted or pushed again**, whatever failed: a fix is always the next version.

- **The gate fails**: nothing was built or published. The tag stays. Fix what it names (the fixture, the CHANGELOG section, the version) on `main` and release the next patch (`X.Y.Z+1`, or the next `rc.N`), whose CHANGELOG section says that `X.Y.Z` wasn't released. Running `sh scripts/release-gate.sh vX.Y.Z` before tagging (step 3) avoids this.
- **A build or test job fails**: nothing was published. If it was the runner (a timeout, a download), rerun the failed jobs: `gh run rerun <run id> --failed`. If the code is at fault, the tag stays: fix it on `main` and release the next patch, as above.
- **Publish fails** while uploading: the release is a draft that nobody sees. Delete the draft (`gh release delete vX.Y.Z -R p-sodmann/ironweaver-db --yes`, which keeps the tag) and rerun the publish job.
- **Something is wrong with a published release**: **never retag** and never replace its files. Fix it on `main` and release the next patch. If the release is harmful (data loss, a security problem), edit its notes to say so and point to the fix; don't delete it, since people may have checked its checksums.
