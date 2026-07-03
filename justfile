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
	cd web-ui && npm run typecheck

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

# Serve the website locally with live reload; pass `true` to enable comment review mode (`just docs-serve true`)
[group('docs')]
docs-serve comment="false":
	wcl wdoc serve docs/main.wcl {{ if comment == "true" { "--comment" } else { "" } }}

# Regenerate the Claude Code skill at .claude/skills/vmlab from the wskill (single source)
[group('docs')]
skill-build: wskill-check
	wcl wdoc skill docs/wskills/vmlab/wdoc/skill/main.wcl --out .claude/skills/vmlab

# Remove generated site + wskill projections
[group('docs')]
docs-clean:
	rm -rf docs/_site docs/wskills/vmlab/out

# Install the SolidJS web UI's npm dependencies
[group('web')]
web-ui-install:
	cd web-ui && npm install

# Build the web UI to web-ui/dist (embedded into vmlab-web at compile time)
[group('web')]
web-ui-build:
	cd web-ui && npm run build

# Build the vmlab-web binary (frontend first, then the embedded server)
[group('web')]
web-build: web-ui-build
	cargo build --features web --bin vmlab-web

# Build and run vmlab-web against the lab in `dir` (Ctrl-C to stop)
[group('web')]
web-serve dir='examples/mixed-lab': web-build
	cd {{dir}} && {{justfile_directory()}}/target/debug/vmlab-web

# Stop any running vmlab-web server (useful when it was started in the background)
[group('web')]
web-stop:
	pkill -x vmlab-web && echo "vmlab-web stopped" || echo "vmlab-web not running"

# Build + start the Docker web UI stack (serves the UI on :7878)
[group('web')]
compose-up:
	docker compose up --build

# Stop and remove the Docker web UI stack
[group('web')]
compose-down:
	docker compose down

# Run the Vite dev server with hot reload (proxies to a running vmlab-web on :7878)
[group('web')]
web-dev:
	cd web-ui && npm run dev
