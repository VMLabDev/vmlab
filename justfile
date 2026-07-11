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

# The eBPF fast-path programs (ebpf/ workspace) need the nightly pinned in
# ebpf/rust-toolchain.toml plus bpf-linker built against that same toolchain
# (its LLVM proxy dlopens the toolchain's libLLVM — a mismatched install
# falls back to whatever LLVM the host has, breaking reproducibility).

# Install the pinned bpf-linker for the ebpf toolchain (one-time setup)
[group('build')]
ebpf-tools:
	cd ebpf && rustup run "$(grep '^channel' rust-toolchain.toml | cut -d'"' -f2)" \
		cargo install bpf-linker --version 0.10.3 --locked --force

# Rebuild the committed BPF fast-path objects from ebpf/ (pinned toolchain)
[group('build')]
ebpf-build:
	cd ebpf && cargo test -p fastpath-logic
	cd ebpf && cargo build --release --target bpfel-unknown-none -Z build-std=core \
		-p fastpath-sockmap -p xdp-switch
	cp ebpf/target/bpfel-unknown-none/release/fastpath-sockmap src/net/fastpath/bpf/fastpath_sockmap.bpf.o
	cp ebpf/target/bpfel-unknown-none/release/xdp-switch src/net/fastpath/bpf/xdp_switch.bpf.o

# Verify the committed BPF objects match the ebpf/ sources (CI)
[group('check')]
ebpf-verify: ebpf-build
	git diff --exit-code src/net/fastpath/bpf/

# Run the privileged fast-path integration tests (kernel splice + XDP; sudo
# prompts once). The tier is a per-process singleton, so each tier gets its
# own invocation of the test binary with VMLAB_FASTPATH forced.
[group('test')]
fastpath-test:
	#!/usr/bin/env bash
	set -euo pipefail
	bin=$(cargo test --lib --no-run 2>&1 | sed -n 's|.*Executable unittests src/lib.rs (\(.*\))$|\1|p')
	[ -n "$bin" ] || { echo "could not locate the test binary"; exit 1; }
	sudo VMLAB_FASTPATH=sockmap "$bin" fastpath_sockmap --ignored --test-threads=1
	sudo VMLAB_FASTPATH=afxdp "$bin" fastpath_afxdp --ignored --test-threads=1

# A/B throughput smoke: the same frame pump with the fast path off vs on
[group('test')]
fastpath-bench:
	#!/usr/bin/env bash
	set -euo pipefail
	bin=$(cargo test --release --lib --no-run 2>&1 | sed -n 's|.*Executable unittests src/lib.rs (\(.*\))$|\1|p')
	[ -n "$bin" ] || { echo "could not locate the test binary"; exit 1; }
	echo "--- userspace ---"
	VMLAB_FASTPATH=off "$bin" fastpath_bench_ab --ignored --nocapture --test-threads=1
	echo "--- sockmap (skipped without CAP_BPF/CAP_NET_ADMIN) ---"
	sudo VMLAB_FASTPATH=sockmap "$bin" fastpath_bench_ab --ignored --nocapture --test-threads=1

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

# Render the wskill book to docs/help — embedded into vmlab-web as the in-app /help
[group('docs')]
help-build:
	wcl wdoc build docs/wskills/vmlab/wdoc/book/main.wcl --out docs/help

# Remove generated site + wskill projections + the in-app help render
[group('docs')]
docs-clean:
	rm -rf docs/_site docs/wskills/vmlab/out
	find docs/help -mindepth 1 -not -name .gitkeep -delete

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

# Build the vmlab-web binary (frontend + help book first, then the embedded server)
[group('web')]
web-build: web-ui-build help-build
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

# Launch two isolated vmlab instances (peer-a on :7871, peer-b on :7872, state under .vmlab-peer-demo/) bridged by a local cross-host trunk — the remote-vmlab peering demo
[group('web')]
peer-demo: web-build build
	#!/usr/bin/env bash
	set -euo pipefail
	root="{{justfile_directory()}}"
	for side in a b; do
		base="$root/.vmlab-peer-demo/$side"
		mkdir -p "$base/run" "$base/state" "$base/config/vmlab"
		if [ "$side" = a ]; then trunk=13947; web=7871; else trunk=13948; web=7872; fi
		printf 'import <vmlab-host.wcl>\nhost {\n  psk        = "peer-demo"\n  trunk_port = %s\n}\n' "$trunk" \
			> "$base/config/vmlab/config.wcl"
		# The XDG env separates every socket/registry/log between the two
		# instances and is inherited by the daemons vmlab-web spawns;
		# XDG_DATA_HOME stays shared so one template pull serves both sides.
		( cd "$root/examples/peer-$side" && \
			XDG_RUNTIME_DIR="$base/run" XDG_STATE_HOME="$base/state" XDG_CONFIG_HOME="$base/config" \
			"$root/target/debug/vmlab-web" --port "$web" >>"$base/web.log" 2>&1 & \
			echo $! > "$base/web.pid" )
		echo "peer-$side: http://localhost:$web   (trunk :$trunk, home $base)"
	done
	echo "bring both labs up, then:  watch the remote-vmlab LEDs — and stop one side to see them drop"

# Stop the peer-demo instances (web servers + their supervisors/lab daemons)
[group('web')]
peer-demo-stop:
	#!/usr/bin/env bash
	root="{{justfile_directory()}}"
	for side in a b; do
		base="$root/.vmlab-peer-demo/$side"
		if [ -f "$base/web.pid" ] && kill "$(cat "$base/web.pid")" 2>/dev/null; then
			echo "peer-$side web stopped"
		else
			echo "peer-$side web not running"
		fi
		rm -f "$base/web.pid"
		XDG_RUNTIME_DIR="$base/run" XDG_STATE_HOME="$base/state" XDG_CONFIG_HOME="$base/config" \
			"$root/target/debug/vmlab" daemon stop >/dev/null 2>&1 || true
	done

# Build + start the Docker web UI stack (serves the UI on :7878)
[group('web')]
compose-up:
	docker compose up --build

# Rebuild the runtime image from the current tree (host + guest asset) and (re)start the stack detached; follow with `docker compose logs -f`
[group('web')]
compose-rebuild:
	docker compose up -d --build --force-recreate
	@echo "web UI: http://localhost:7878"

# Stop and remove the Docker web UI stack
[group('web')]
compose-down:
	docker compose down

# Run the Vite dev server with hot reload (proxies to a running vmlab-web on :7878)
[group('web')]
web-dev:
	cd web-ui && pnpm dev
