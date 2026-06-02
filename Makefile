.PHONY: build test

build:
	cargo build --release

test:
	cargo test --release
