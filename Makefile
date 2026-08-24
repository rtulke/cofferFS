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

# Building as root (typically via `sudo make install` run directly, without
# ./setup.sh) doesn't work reliably: root has no Rust toolchain of its own,
# and some setups don't even let root read into a locked-down $HOME - so
# skip the auto-build in that case and fail with a clear next step instead
# of letting cargo's confusing "could not find Cargo.toml" surface.
install: man
	@if [ "$$(id -u)" -eq 0 ]; then \
		if [ ! -r target/release/cryptc ]; then \
			echo "error: target/release/cryptc is missing or unreadable as root." >&2; \
			echo "Build it as your normal user first, then install:" >&2; \
			echo "    make release" >&2; \
			echo "    sudo make install" >&2; \
			echo "Or just run ./setup.sh, which handles this for you." >&2; \
			exit 1; \
		fi; \
	else \
		$(MAKE) release; \
	fi
	install -Dm755 target/release/cryptc $(DESTDIR)$(PREFIX)/bin/cryptc
	install -Dm644 target/man/cryptc.1.gz $(DESTDIR)$(PREFIX)/share/man/man1/cryptc.1.gz

uninstall:
	rm -f $(DESTDIR)$(PREFIX)/bin/cryptc
	rm -f $(DESTDIR)$(PREFIX)/share/man/man1/cryptc.1.gz

test:
	cargo test

clean:
	cargo clean
