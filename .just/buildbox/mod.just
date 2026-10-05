# Release artefacts built in a container: build/Dockerfile holds every
# toolchain (scripts/guest-toolchains.sh, one stage per layer) and none of the
# checkout. build/run.sh bind-mounts the checkout and runs as the host uid:gid,
# with named volumes for the cargo home and every target dir, so output is
# never root-owned and rebuilds are incremental. See build/README.md.

set working-directory := '../..'

# Override per checkout (VMLAB_BUILD_IMAGE), like VMLAB_E2E_IMAGE.
image := env('VMLAB_BUILD_IMAGE', 'vmlab-build')

export VMLAB_BUILD_IMAGE := image

[default, private]
main:
	@just --list buildbox

[private]
require-docker:
	@command -v docker > /dev/null 2>&1 || (echo 'docker is required for the build container' && exit 1)

# Build the vmlab-build image (toolchains only; the checkout is mounted at run time)
[group('build')]
image: require-docker
	DOCKER_BUILDKIT=1 docker build -f build/Dockerfile -t {{image}} .

# Build the release binary, the guest bundle and the BPF objects into target/dist/ (version defaults to Cargo.toml's)
[group('build')]
dist version='': image
	./build/run.sh ./build/dist.sh {{version}}

# Open a shell in the build container over this checkout (`just ci::check` runs there)
[group('build')]
shell: image
	./build/run.sh -t bash
