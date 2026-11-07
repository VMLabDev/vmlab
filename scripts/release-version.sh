#!/usr/bin/env bash
# Calculate the release version of HEAD for the "Calculate version" job in
# .github/workflows/ci.yml (VML-104). Run it in a full checkout (fetch-depth 0)
# with the tags fetched.
#
# Standard output: one key=value line each for do_release, version, prerelease,
# bump and last_tag. Nothing else goes to standard output. Diagnostics go to
# standard error.
#
# Rules:
#   - Neither trailer on HEAD: do_release=false, and nothing else is printed.
#   - release: true on HEAD (not pre-release) and the last v* tag has a suffix
#     (vX.Y.Z-alpha): promote it. The version is X.Y.Z, with no bump.
#   - Otherwise bump from the last v* tag with its suffix stripped. The bump
#     level comes from the conventional-commit subjects since that tag. In 0.x a
#     major bump is a minor bump.
#   - pre-release: true appends -alpha to the version.
set -eu

trailer() {
  git log -1 --format="%(trailers:key=$1,valueonly)"
}

IS_RELEASE=$(trailer release)
IS_PRERELEASE=$(trailer pre-release)

if [ "$IS_RELEASE" != "true" ] && [ "$IS_PRERELEASE" != "true" ]; then
  echo "No release/pre-release trailer on HEAD: tests only." >&2
  echo "do_release=false"
  exit 0
fi

LAST_TAG=$(git describe --tags --match 'v*' --abbrev=0 2>/dev/null || echo "")
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
  MAJOR=$PREV_MAJOR; MINOR=$PREV_MINOR; PATCH=$PREV_PATCH
else
  BUMP="none"
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

echo "Calculated version: $VERSION (bump=$BUMP, prerelease=$PRERELEASE, from ${LAST_TAG:-none})" >&2
echo "do_release=true"
echo "version=$VERSION"
echo "prerelease=$PRERELEASE"
echo "bump=$BUMP"
echo "last_tag=$LAST_TAG"
