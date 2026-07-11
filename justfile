[default, private]
main:
	@just --list

# Build the project (debug)
[group('build')]
build:
	cargo build

# Build release artifacts
[group('build')]
release:
	cargo build --release

# Run the test suite
[group('test')]
test:
	cargo test

# Run clippy with warnings as errors (--all-features covers the web binary)
[group('check')]
lint:
	cargo clippy --all-targets --all-features -- -D warnings

# Verify formatting without changing files
[group('check')]
fmt-check:
	cargo fmt --check

# Format the codebase
[group('check')]
fmt:
	cargo fmt

# Typecheck the web UI (`vite build` strips types without checking them)
[group('check')]
web-ui-check:
	cd web-ui && pnpm typecheck

# Lint, format check, tests, and the web UI typecheck
[group('check')]
check: lint fmt-check test web-ui-check

# Build the official runtime container image: `vmlab` CLI + `vmlab-web` (PRD §14)
[group('build')]
image tag='vmlab:latest':
	docker build -t {{tag}} -f Containerfile .

# Install the vmlab binary into the user profile (~/.cargo/bin)
[group('build')]
install:
	cargo install --path . --locked

# Build the container micro-VM guest asset (kernel + initramfs, PRD §18)
[group('build')]
guest-build arch='x86_64 aarch64':
	./guest/build-asset.sh {{arch}}

# Build + install the guest asset into ~/.local/share/vmlab/guest
[group('build')]
guest-install: guest-build
	mkdir -p ~/.local/share/vmlab/guest
	cp -r guest/dist/* ~/.local/share/vmlab/guest/

# Bring a lab up (a VNC viewer opens per VM when the lab sets `gui = true`)
[group('lab')]
lab-up dir='examples/mixed-lab': release
	cd {{dir}} && {{justfile_directory()}}/target/release/vmlab up

# Stop a running lab gracefully (clones retained)
[group('lab')]
lab-down dir='examples/mixed-lab': release
	cd {{dir}} && {{justfile_directory()}}/target/release/vmlab down

# Tear a lab down completely: stop + delete clones and lab-local state
[group('lab')]
lab-destroy dir='examples/mixed-lab': release
	cd {{dir}} && {{justfile_directory()}}/target/release/vmlab destroy

# Launch the winsrv-desktop example (opens the WS2025 guest window)
[group('lab')]
winsrv-desktop: (lab-up 'examples/winsrv-desktop')

# The website + vmlab wskill are authored in wdoc and rendered by the `wcl` CLI.
# Install it from https://wcl.dev (or `cargo install --git …/wcl wcl`).

# Validate the vmlab wskill model and every projection template
[group('docs')]
wskill-check:
	wcl check docs/wskills/vmlab/wskill.wcl
	wcl check docs/wskills/vmlab/wdoc/book/main.wcl
	wcl check docs/wskills/vmlab/wdoc/skill/main.wcl
	wcl check docs/wskills/vmlab/wdoc/presentation/main.wcl
	wcl check docs/wskills/vmlab/wdoc/training/main.wcl

# Build the documentation website to docs/_site (landing + embedded reference book, deck, and course)
[group('docs')]
docs-build: wskill-check
	wcl wdoc build docs/main.wcl --out docs/_site

# Serve the website locally with live reload; pass `true` to enable comment review mode, and a port to pin one (`just docs-serve true 9090`). Default `auto` picks the first free port near 8080 and prints the URL
[group('docs')]
docs-serve comment="false" port="auto":
	wcl wdoc serve docs/main.wcl --addr {{ if port == "auto" { "auto" } else { "127.0.0.1:" + port } }} {{ if comment == "true" { "--comment" } else { "" } }}

# Regenerate the Claude Code skill at .claude/skills/vmlab from the wskill (single source)
[group('docs')]
skill-build: wskill-check
	wcl wdoc skill docs/wskills/vmlab/wdoc/skill/main.wcl --out .claude/skills/vmlab

# Remove generated site + wskill projections
[group('docs')]
docs-clean:
	rm -rf docs/_site docs/wskills/vmlab/out

# Install the SolidJS web UI's pnpm dependencies (@forge/* are git-subdir deps)
[group('web')]
web-ui-install:
	cd web-ui && pnpm install

# Build the web UI to web-ui/dist (embedded into vmlab-web at compile time)
[group('web')]
web-ui-build:
	cd web-ui && pnpm build

# Pin the @forge/* deps to a new forge rev (updates package.json + the
# pnpm-workspace.yaml prepare allowlist together, then reinstalls)
[group('web')]
web-ui-forge-bump rev:
	cd web-ui && sed -i -E 's|(github:wiltaylor/forge#)[0-9a-f]{40}|\1{{rev}}|g' package.json && \
		sed -i -E 's|(codeload.github.com/wiltaylor/forge/tar.gz/)[0-9a-f]{40}|\1{{rev}}|g; s|(github:wiltaylor/forge#)[0-9a-f]{40}|\1{{rev}}|g' pnpm-workspace.yaml && \
		pnpm install

# Build the vmlab-web binary (frontend first, then the embedded server)
[group('web')]
web-build: web-ui-build
	cargo build --features web --bin vmlab-web

# Build and run vmlab-web against the lab in `dir` (Ctrl-C to stop)
[group('web')]
web-serve dir='examples/mixed-lab': web-build
	cd {{dir}} && {{justfile_directory()}}/target/debug/vmlab-web

# Rebuild/install the x86_64 guest and run vmlab-web; restart existing containers from the UI.
[group('web')]
web-serve-guest dir='examples/mixed-lab': web-build (guest-build 'x86_64')
	mkdir -p ~/.local/share/vmlab/guest/x86_64
	cp -r guest/dist/x86_64/. ~/.local/share/vmlab/guest/x86_64/
	cd {{dir}} && {{justfile_directory()}}/target/debug/vmlab-web

# Stop any running vmlab-web server (useful when it was started in the background)
[group('web')]
web-stop:
	pkill -x vmlab-web && echo "vmlab-web stopped" || echo "vmlab-web not running"

# Build + start the Docker web UI stack (serves the UI on :7878)
[group('web')]
compose-up:
	docker compose up --build

# Rebuild the runtime image from the current tree (host + guest asset) and (re)start the stack detached; follow with `docker compose logs -f`
[group('web')]
compose-rebuild:
	docker compose up -d --build --force-recreate

# Stop and remove the Docker web UI stack
[group('web')]
compose-down:
	docker compose down

# Run the Vite dev server with hot reload (proxies to a running vmlab-web on :7878)
[group('web')]
web-dev:
	cd web-ui && pnpm dev
