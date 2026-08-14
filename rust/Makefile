PREFIX ?= /usr/local

.PHONY: build release debug install uninstall clean test

build: release

release:
	cargo build --release

debug:
	cargo build

install: release
	install -Dm755 target/release/cryptc $(DESTDIR)$(PREFIX)/bin/cryptc

uninstall:
	rm -f $(DESTDIR)$(PREFIX)/bin/cryptc

test:
	cargo test

clean:
	cargo clean
