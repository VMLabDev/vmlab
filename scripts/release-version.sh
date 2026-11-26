#!/usr/bin/env bash
# Calculate the release version of HEAD, for `just release::local` and
# `just release::version` (VML-104, card 423). Run it in a full checkout with
# the tags fetched.
#
# Usage: release-version.sh [--pre-release | --release] [--bump major|minor|patch]
#
# With --pre-release or --release the kind and the bump come from the command
# line, and the trailers of HEAD are not read. Without either, the trailers
# `pre-release: true`, `release: true` and `bump:` of HEAD decide, as before.
#
# Standard output: one key=value line each for do_release, version, prerelease,
# bump and last_tag. Nothing else goes to standard output. Diagnostics go to
# standard error.
#
# Rules:
#   - No kind on the command line and neither trailer on HEAD:
#     do_release=false, and nothing else is printed.
#   - A release (not a pre-release) and the highest version tag has a suffix
#     (vX.Y.Z-alpha): promote it. The version is X.Y.Z, with no bump.
#   - Otherwise strip the suffix from the highest version tag and bump it.
#     The level comes from --bump, HEAD's bump trailer, or the conventional
#     commit subjects since that tag. In 0.x a major bump is a minor bump.
#   - A pre-release appends -alpha to the version.
set -eu

trailer() {
  git log -1 --format="%(trailers:key=$1,valueonly)"
}

KIND=""
ARG_BUMP=""
while [ $# -gt 0 ]; do
  case "$1" in
    --pre-release|--release)
      [ -z "$KIND" ] || { echo "release-version: give --pre-release or --release, not both" >&2; exit 2; }
      KIND="${1#--}"; shift ;;
    --bump)
      [ $# -ge 2 ] || { echo "release-version: --bump needs major, minor or patch" >&2; exit 2; }
      ARG_BUMP="$2"; shift 2 ;;
    --bump=*) ARG_BUMP="${1#--bump=}"; shift ;;
    *) echo "release-version: unknown argument: $1" >&2; exit 2 ;;
  esac
done
case "$ARG_BUMP" in
  ""|major|minor|patch) ;;
  *) echo "release-version: --bump is major, minor or patch, not '$ARG_BUMP'" >&2; exit 2 ;;
esac
if [ -z "$KIND" ] && [ -n "$ARG_BUMP" ]; then
  echo "release-version: --bump needs --pre-release or --release" >&2
  exit 2
fi

case "$KIND" in
  pre-release) IS_RELEASE=""; IS_PRERELEASE=true ;;
  release) IS_RELEASE=true; IS_PRERELEASE="" ;;
  *) IS_RELEASE=$(trailer release); IS_PRERELEASE=$(trailer pre-release) ;;
esac

if [ "$IS_RELEASE" != "true" ] && [ "$IS_PRERELEASE" != "true" ]; then
  echo "No release/pre-release trailer on HEAD and no kind on the command line: no release." >&2
  echo "do_release=false"
  exit 0
fi

# Published tags can sit on main's merge commits outside HEAD's ancestry.
# A stable tag sorts ahead of the alpha with the same version number.
TAGS=$(git -c versionsort.suffix=-alpha tag --list 'v[0-9]*' --sort=-version:refname)
LAST_TAG=$(printf '%s\n' "$TAGS" | awk '/^v[0-9]+\.[0-9]+\.[0-9]+(-alpha)?$/ {print; exit}')
VER=""
BASE=""
if [ -z "$LAST_TAG" ]; then
  PREV_MAJOR=0; PREV_MINOR=0; PREV_PATCH=0; RANGE="HEAD"
else
  VER="${LAST_TAG#v}"
  BASE="${VER%%-*}"
  IFS='.' read -r PREV_MAJOR PREV_MINOR PREV_PATCH <<< "$BASE"
  RANGE="${LAST_TAG}..HEAD"
fi

if [ "$IS_RELEASE" = "true" ] && [ "$IS_PRERELEASE" != "true" ] \
  && [ -n "$LAST_TAG" ] && [ "$VER" != "$BASE" ]; then
  # Promote the pre-release: the same version without its suffix.
  BUMP="promote"
  SOURCE="pre-release promotion"
  MAJOR=$PREV_MAJOR; MINOR=$PREV_MINOR; PATCH=$PREV_PATCH
else
  if [ -n "$KIND" ]; then BUMP=$ARG_BUMP; BUMP_SOURCE="command line"
  else BUMP=$(trailer bump); BUMP_SOURCE="HEAD bump trailer"; fi
  case "$BUMP" in
    major|minor|patch) SOURCE=$BUMP_SOURCE ;;
    *) BUMP="none"; SOURCE="commit subjects" ;;
  esac
  if [ "$BUMP" = "none" ]; then
  while IFS= read -r HASH; do
    SUBJECT=$(git log -1 --format='%s' "$HASH")
    BODY=$(git log -1 --format='%b' "$HASH")
    if printf '%s\n' "$SUBJECT" | grep -qE '^[a-z]+(\(.+\))?!:' \
      || printf '%s\n' "$BODY" | grep -qiE '^BREAKING[ -]CHANGE:'; then
      BUMP="major"
    elif printf '%s\n' "$SUBJECT" | grep -qE '^feat(\(.+\))?:' && [ "$BUMP" != "major" ]; then
      BUMP="minor"
    elif printf '%s\n' "$SUBJECT" | grep -qE '^(fix|perf|refactor|build|ci|docs|style|test|chore|revert)(\(.+\))?:' && [ "$BUMP" = "none" ]; then
      BUMP="patch"
    fi
  done < <(git log --format='%H' "$RANGE")
  if [ "$BUMP" = "none" ]; then
    BUMP="patch"
  fi
  fi

  case "$BUMP" in
    major)
      if [ "$PREV_MAJOR" -eq 0 ]; then MAJOR=0; MINOR=$((PREV_MINOR + 1)); PATCH=0
      else MAJOR=$((PREV_MAJOR + 1)); MINOR=0; PATCH=0; fi ;;
    minor) MAJOR=$PREV_MAJOR; MINOR=$((PREV_MINOR + 1)); PATCH=0 ;;
    patch) MAJOR=$PREV_MAJOR; MINOR=$PREV_MINOR; PATCH=$((PREV_PATCH + 1)) ;;
  esac
fi

VERSION="${MAJOR}.${MINOR}.${PATCH}"
PRERELEASE=false
if [ "$IS_PRERELEASE" = "true" ]; then
  VERSION="${VERSION}-alpha"
  PRERELEASE=true
fi

echo "Calculated version: $VERSION (bump=$BUMP, source=$SOURCE, prerelease=$PRERELEASE, from ${LAST_TAG:-none})" >&2
echo "do_release=true"
echo "version=$VERSION"
echo "prerelease=$PRERELEASE"
echo "bump=$BUMP"
echo "last_tag=$LAST_TAG"
