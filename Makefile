# arbitrage-rs common tasks. `make pre-push` must pass before every push.

MSRV := 1.88

.PHONY: all
all: pre-push

.PHONY: build
build:
	cargo build

.PHONY: test
test:
	cargo test --no-default-features --all-targets
	cargo test --no-default-features --doc
	cargo test --all-features --all-targets
	cargo test --all-features --doc

# The library alone, without the dev-dependencies, whose features unify into
# every test build.
.PHONY: lib-check
lib-check:
	cargo check --no-default-features --lib
	cargo check --no-default-features --features instruments --lib

.PHONY: fmt
fmt:
	cargo fmt --all

.PHONY: fmt-check
fmt-check:
	cargo fmt --all --check

.PHONY: lint
lint:
	cargo clippy --all-targets --all-features -- -D warnings

.PHONY: doc
doc:
	RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
	RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features

.PHONY: msrv
msrv:
	cargo +$(MSRV) check --all-targets
	cargo +$(MSRV) check --all-targets --all-features

.PHONY: pre-push
pre-push: fmt-check lint lib-check test doc msrv

.PHONY: clean
clean:
	cargo clean
