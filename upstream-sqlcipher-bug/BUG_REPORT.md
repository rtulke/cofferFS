## `PRAGMA cipher_integrity_check` reports false HMAC failures for every page beyond 4GB (32-bit overflow in offset calculation)

### Summary

`PRAGMA cipher_integrity_check` reports every page as HMAC-verification-failed
once a database exceeds 4GB (2^32 bytes). The database itself is **not**
corrupt — `PRAGMA integrity_check` (which actually decrypts and walks the
B-tree) passes cleanly, and previously-written data reads back correctly and
deterministically. The bug is isolated to `cipher_integrity_check`'s own
page-offset calculation.

### Environment

- SQLCipher 4.5.6 (`sqlite_source_id 3.44.2`)
- Reproduced identically across three independent builds:
  1. Ubuntu 24.04 `libsqlcipher-dev` 4.5.6-1build2 (apt)
  2. Python's `sqlcipher3-binary` PyPI wheel (separately compiled/bundled)
  3. A from-source build (Rust's `rusqlite` crate, `bundled-sqlcipher-vendored-openssl` feature)
- Reproduced with both `cipher_page_size` left at default and explicitly set to 4096.
- Platform: Linux x86_64.

### Root cause

In `src/crypto.c`, `sqlcipher_codec_ctx_integrity_check()`:

```c
static int sqlcipher_codec_ctx_integrity_check(codec_ctx *ctx, Parse *pParse, char *column) {
  Pgno page = 1;                                   /* Pgno = typedef u32 */
  ...
  for(page = 1; page <= file_sz / ctx->page_sz; page++) {
    i64 offset = (page - 1) * ctx->page_sz;         /* <-- overflow happens here */
```

`page` is `Pgno` (`u32`) and `ctx->page_sz` is a plain `int` (both 32-bit).
`(page - 1) * ctx->page_sz` is therefore evaluated entirely in 32-bit
arithmetic under C's usual arithmetic conversions, and the result is only
widened to the 64-bit `i64 offset` *after* the multiplication has already
happened — and already overflowed.

At the default 4096-byte page size, this wraps exactly at page 1,048,577:

```
(1,048,577 - 1) * 4096 = 4,294,967,296 = 2^32  →  wraps to 0 in 32-bit arithmetic
```

From that page onward, `sqlite3OsRead()` is called with a wrapped (wrong)
offset, so the bytes read never match the page that was actually written
there, and the recomputed HMAC never matches the stored one — for every
single page from that point to the end of the file. This exactly matches
the observed symptom: zero corrupt pages below the 4GB mark, every page
"corrupt" above it.

A secondary, minor issue in the same function's error path:

```c
result = sqlite3_mprintf("error reading %d bytes from file page %d at offset %d", read_sz, page, offset);
```

`offset` is `i64` but formatted with `%d`. Only triggered if `sqlite3OsRead`
itself fails (not the main symptom here, but worth fixing alongside).

### Suggested fix

Cast one operand to 64-bit before multiplying, so the multiplication itself
happens in 64-bit arithmetic:

```c
i64 offset = (i64)(page - 1) * ctx->page_sz;
```

And for the format-string issue:

```c
result = sqlite3_mprintf("error reading %d bytes from file page %d at offset %lld", read_sz, page, offset);
```

### Reproduction

Minimal, dependency-free C reproduction attached (`repro_4gb_bug.c`): creates
a SQLCipher database, writes ~5GB via small (100KB) inserts, then runs both
`PRAGMA cipher_integrity_check` and `PRAGMA integrity_check` for comparison.

```
cc -O2 -o repro_4gb_bug repro_4gb_bug.c $(pkg-config --cflags --libs sqlcipher)
./repro_4gb_bug
```

Expected output (abridged):

```
done writing: 53688 rows, 5.37 GB
running PRAGMA cipher_integrity_check...
  HMAC verification failed for page 1048577
  HMAC verification failed for page 1048578
  HMAC verification failed for page 1048579
cipher_integrity_check: 293610 pages reported corrupt (first at page 1048577)
running PRAGMA integrity_check (the real structural/decrypt check)...
  ok
```

`integrity_check` reporting `ok` while `cipher_integrity_check` reports
hundreds of thousands of "corrupt" pages, all above the exact 4GB/page_size
boundary, is the key signature of this bug.

### Impact

Any application that relies on `cipher_integrity_check` as a health check for
databases that can grow past 4GB (backup verification, periodic health
checks, "is my data safe" tooling, etc.) will see false corruption alarms at
that size, which is actively misleading for exactly the kind of tooling this
PRAGMA exists to support.

### Suggested title for the tracker

`cipher_integrity_check: false HMAC failures for all pages past 4GB (32-bit overflow in offset calc, src/crypto.c)`
