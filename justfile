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

# Run clippy with warnings as errors
[group('check')]
lint:
	cargo clippy --all-targets -- -D warnings

# Verify formatting without changing files
[group('check')]
fmt-check:
	cargo fmt --check

# Format the codebase
[group('check')]
fmt:
	cargo fmt

# Lint, format check, and tests
[group('check')]
check: lint fmt-check test

# Context is the parent directory so the sibling WCL/wisp path deps are
# available to the build.
# Build the official runtime container image (PRD §14)
[group('build')]
image tag='vmlab:latest':
	docker build -t {{tag}} -f Containerfile ..

# Bring a lab up (guest windows open when the lab sets `gui = true`)
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
