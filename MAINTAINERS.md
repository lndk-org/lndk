# Guide for maintainers of LNDK

## Release process

We prefer having the entire lifecycle of the release process happen within GitHub actions so that there are no "loose ends"
or inconsistencies between releases based on local environments.

### Prerequisites

```shell
cargo install --locked cargo-dist cargo-release
```

### Use of `cargo-dist` in this project

This project uses [cargo-dist] for automating the release process as much as possible.
Release builds happen via GitHub Actions and the workflow can be seen [here](.github/workflows/release.yml). The file is generated
by `cargo-dist` and should generally not be modified manually. The `Cargo.toml` file also includes manifest config for `cargo-dist`.

When a new version of `cargo-dist` is released and installed, first run

```shell
cargo dist init
```

to update the manifest, and then

```shell
cargo dist generate
```

to regenerate the `release.yml` workflow.

### On day of new release

Make sure that [git-cliff](https://github.com/orhun/git-cliff) is installed for changelog generation.

We'll be using `cargo-release` with a pull request in tandem with `cargo-dist` to create a new release.
Specifically, we'll use a deferred approach instead of `cargo-release`'s default so that we avoid directly
pushing changes to the `master` branch.

As an example, assume we want to release `v0.0.1` of LNDK. The following steps (modified from [the cargo-dist book]) must be followed:

```shell
# Create environment variables for the release branch and version
export VERSION="0.0.1"  # note this must exclude the 'v' prefix
export BRANCH="release-${VERSION}"

# step 0: make a branch
git checkout -b $BRANCH


# step 1: Generate the updates to the changelog and make any other changes needed.
git cliff --unreleased --tag $VERSION --prepend CHANGELOG.md
git commit -am "chore: Prepare ${VERSION} release"


# step 2: have cargo-release handle tedious mechanical stuff
# this will:
#  * do some safety checks like "git index is clean"
#  * update version numbers in your crates (and handle inter-dependencies)
#  * git commit -am "chore: release $NAME $VERSION" (one commit for the whole workspace)
#  * git push (remember we're on a branch)
cargo release --no-publish --no-tag --allow-branch=$BRANCH $VERSION --execute


# step 3: open a PR and review/merge to master
# NOTE: the above steps will result in two commits
#       we recommend using github's "merge and squash" feature to clean up


# step 4: Run cargo-release on master branch
# this will:
#  * tag the commit
#  * push the tag
#  * publish all crates to crates.io (handles waiting for dep publishes to propagate)
#  * trigger cargo-dist when it sees the tag (if applicable)
# THIS WON'T CREATE NEW COMMITS
#
# running "cargo dist plan" is totally optional, but this is is the best time to check
# that your cargo-dist release CI will produce the desired result when you push the tag
git checkout master
git pull
cargo dist plan
cargo release --no-publish

```
[cargo-dist]: https://opensource.axo.dev/cargo-dist
[the cargo-dist book]: https://opensource.axo.dev/cargo-dist/book/workspaces/cargo-release-guide.html#using-cargo-release-with-pull-requests
