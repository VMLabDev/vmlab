# The end-to-end suite: vmlab built from this checkout, run in a privileged
# container with /dev/kvm, exercising every feature in e2e/features.py.
#
# A named volume keeps vmlab's downloaded artefacts (the Alpine cloud image)
# between runs, so only the first run downloads it. It covers `artefacts/`
# alone: build staging has to share a filesystem with the template store.

set working-directory := '../..'

import '../shared.just'

# Override per checkout (VMLAB_E2E_IMAGE) when several worktrees run the suite at once.
image := env('VMLAB_E2E_IMAGE', 'vmlab-e2e')
cache := 'vmlab-e2e-cache'

[default, private]
main:
	@just --list e2e

[private]
require-docker:
	@command -v docker > /dev/null 2>&1 || (echo 'docker is required for the e2e suite' && exit 1)
	@test -c /dev/kvm || (echo '/dev/kvm is required for the e2e suite' && exit 1)

# Build the e2e image from this checkout
[group('e2e')]
image-build: require-docker
	DOCKER_BUILDKIT=1 docker build -f e2e/Dockerfile -t {{image}} .

# Run the suite (arguments go to e2e/run.py: `--only core dev`, `--keep`, `--list`)
[group('e2e')]
run *args: image-build
	mkdir -p e2e/results
	docker run --rm --init --privileged --device /dev/kvm \
		-v {{cache}}:/root/.local/share/vmlab/cache/artefacts \
		-v "$PWD/e2e:/e2e:ro" \
		-v "$PWD/e2e/results:/results" \
		$(test -d "$HOME/.local/share/config-weave/bin" && echo "-v $HOME/.local/share/config-weave/bin:/opt/config-weave:ro -e VMLAB_CONFIG_WEAVE_DIR=/opt/config-weave") \
		{{image}} {{args}}

# Drop the download cache volume
[group('e2e')]
cache-clean: require-docker
	docker volume rm {{cache}}
