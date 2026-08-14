PREFIX ?= /usr/local

.PHONY: build release debug man install uninstall clean test

build: release

release:
	cargo build --release

debug:
	cargo build

# gzip -n: no embedded filename/timestamp, so the compressed man page is
# byte-identical across rebuilds (matches Debian's reproducible-builds and
# lintian's manpage-not-compressed-with-max-compression expectations).
man:
	mkdir -p target/man
	gzip -9 -n -c packaging/cryptc.1 > target/man/cryptc.1.gz

install: release man
	install -Dm755 target/release/cryptc $(DESTDIR)$(PREFIX)/bin/cryptc
	install -Dm644 target/man/cryptc.1.gz $(DESTDIR)$(PREFIX)/share/man/man1/cryptc.1.gz

uninstall:
	rm -f $(DESTDIR)$(PREFIX)/bin/cryptc
	rm -f $(DESTDIR)$(PREFIX)/share/man/man1/cryptc.1.gz

test:
	cargo test

clean:
	cargo clean
