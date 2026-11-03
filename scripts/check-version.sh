#!/usr/bin/env sh
# Fails unless Cargo.toml's version is newer than every released v* tag, so
# main is always ready to be released from GitHub as v<that version>.
# Needs the repository's tags (CI: actions/checkout with fetch-depth: 0).
set -eu
cd "$(dirname "$0")/.."

version="$(awk -F'"' '/^version = / { print $2; exit }' Cargo.toml)"
if [ -z "$version" ]; then
	echo "Could not read version from Cargo.toml" >&2
	exit 1
fi
latest="$(git tag -l 'v[0-9]*' | sed 's/^v//' | sort -V | tail -n 1)"
if [ -z "$latest" ]; then
	echo "No v* tags found; fetch tags (git fetch --tags) before checking." >&2
	exit 1
fi
newest="$(printf '%s\n%s\n' "$latest" "$version" | sort -V | tail -n 1)"
if [ "$version" = "$latest" ] || [ "$newest" != "$version" ]; then
	echo "Cargo.toml version $version is not newer than the latest release v$latest." >&2
	echo "Bump version in Cargo.toml (then run cargo build to update Cargo.lock)." >&2
	exit 1
fi
echo "Cargo.toml $version > v$latest: release this from GitHub as tag v$version."
