# arbitrage-rs common tasks. `make pre-push` must pass before every push.

MSRV := 1.88

.PHONY: all
all: pre-push

.PHONY: build
build:
	cargo build

.PHONY: test
test:
	cargo test --all-targets
	cargo test --doc

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

.PHONY: msrv
msrv:
	cargo +$(MSRV) check --all-targets

.PHONY: pre-push
pre-push: fmt-check lint test doc msrv

.PHONY: clean
clean:
	cargo clean
