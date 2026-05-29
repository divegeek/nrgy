.PHONY: build test clean

build:
	cargo build

test:
	cargo test

clean:
	cargo clean
	rm -rf proto/
	rm -rf src/tesla/proto/
