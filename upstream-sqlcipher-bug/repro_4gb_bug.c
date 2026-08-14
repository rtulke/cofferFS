/*
 * Minimal reproduction: SQLCipher's "PRAGMA cipher_integrity_check" reports
 * false HMAC failures for every page once the database exceeds 4GB.
 *
 * Root cause (found by reading src/crypto.c / the sqlite3.c amalgamation),
 * in sqlcipher_codec_ctx_integrity_check():
 *
 *     Pgno page = 1;             // Pgno is typedef'd as u32
 *     ...
 *     for (page = 1; page <= file_sz / ctx->page_sz; page++) {
 *         i64 offset = (page - 1) * ctx->page_sz;   // <-- overflows in 32-bit
 *                                                    //     BEFORE being widened to i64
 *
 * ctx->page_sz is a plain `int`. (page - 1) * ctx->page_sz is therefore
 * computed entirely in 32-bit arithmetic and wraps at 2^32 bytes (= exactly
 * the 4GB mark, at the default 4096-byte page size: page 1,048,577). Every
 * page from that point on is read from a wrapped-around (wrong) file offset,
 * so its HMAC never matches - even though the page was written and encrypted
 * correctly. The data itself is fine; only this diagnostic PRAGMA is wrong.
 *
 * Suggested fix: cast one operand to i64 before multiplying, e.g.
 *     i64 offset = (i64)(page - 1) * ctx->page_sz;
 *
 * Build:
 *   cc -O2 -o repro_4gb_bug repro_4gb_bug.c $(pkg-config --cflags --libs sqlcipher)
 * Run:
 *   ./repro_4gb_bug            # writes ~5GB to ./repro_4gb_bug.db, then checks it
 *
 * Confirmed on SQLCipher 4.5.6 across three independent builds:
 *   - Ubuntu 24.04 apt package (libsqlcipher-dev 4.5.6-1build2)
 *   - Python's sqlcipher3-binary PyPI wheel (separately bundled build)
 *   - A from-source build via bundled-sqlcipher-vendored-openssl (Rust rusqlite)
 * All three fail at the identical page number for the identical reason.
 */

#include <sqlite3.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define DB_PATH "repro_4gb_bug.db"
#define ROW_BYTES 100000
#define TARGET_BYTES (5LL * 1024 * 1024 * 1024) /* 5GB: comfortably past 4GB */

static void die(sqlite3 *db, const char *what, int rc) {
    fprintf(stderr, "%s failed (rc=%d): %s\n", what, rc, sqlite3_errmsg(db));
    exit(1);
}

int main(void) {
    remove(DB_PATH);
    remove(DB_PATH "-wal");
    remove(DB_PATH "-shm");

    sqlite3 *db;
    int rc = sqlite3_open(DB_PATH, &db);
    if (rc != SQLITE_OK) die(db, "sqlite3_open", rc);

    char *err = NULL;
    sqlite3_exec(db, "PRAGMA key='reprokey';", NULL, NULL, &err);
    sqlite3_exec(db, "PRAGMA cipher_page_size = 4096;", NULL, NULL, &err);
    rc = sqlite3_exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, blob BLOB NOT NULL);", NULL, NULL, &err);
    if (rc != SQLITE_OK) { fprintf(stderr, "create table: %s\n", err); return 1; }
    sqlite3_exec(db, "PRAGMA journal_mode=WAL;", NULL, NULL, &err);
    sqlite3_exec(db, "PRAGMA synchronous=NORMAL;", NULL, NULL, &err);

    sqlite3_stmt *stmt;
    rc = sqlite3_prepare_v2(db, "INSERT INTO t (blob) VALUES (?1)", -1, &stmt, NULL);
    if (rc != SQLITE_OK) die(db, "prepare insert", rc);

    char *chunk = malloc(ROW_BYTES);
    memset(chunk, 0x41, ROW_BYTES);

    long long written = 0;
    long long n = 0;
    while (written < TARGET_BYTES) {
        sqlite3_bind_blob(stmt, 1, chunk, ROW_BYTES, SQLITE_STATIC);
        rc = sqlite3_step(stmt);
        if (rc != SQLITE_DONE) die(db, "insert step", rc);
        sqlite3_reset(stmt);
        written += ROW_BYTES;
        n++;
        if (n % 5000 == 0) {
            printf("%lld rows, %.2f GB\n", n, written / 1e9);
        }
    }
    sqlite3_finalize(stmt);
    printf("done writing: %lld rows, %.2f GB\n", n, written / 1e9);
    free(chunk);

    printf("running PRAGMA cipher_integrity_check...\n");
    sqlite3_stmt *check;
    rc = sqlite3_prepare_v2(db, "PRAGMA cipher_integrity_check", -1, &check, NULL);
    if (rc != SQLITE_OK) die(db, "prepare cipher_integrity_check", rc);

    long long corrupt = 0;
    long long first_bad_page = -1;
    while (sqlite3_step(check) == SQLITE_ROW) {
        const unsigned char *msg = sqlite3_column_text(check, 0);
        corrupt++;
        if (corrupt <= 3) printf("  %s\n", msg);
        if (first_bad_page < 0) {
            const char *p = strrchr((const char *)msg, ' ');
            if (p) first_bad_page = atoll(p + 1);
        }
    }
    sqlite3_finalize(check);
    printf("cipher_integrity_check: %lld pages reported corrupt", corrupt);
    if (first_bad_page > 0) printf(" (first at page %lld)", first_bad_page);
    printf("\n");

    printf("running PRAGMA integrity_check (the real structural/decrypt check)...\n");
    sqlite3_stmt *real_check;
    rc = sqlite3_prepare_v2(db, "PRAGMA integrity_check", -1, &real_check, NULL);
    if (rc != SQLITE_OK) die(db, "prepare integrity_check", rc);
    while (sqlite3_step(real_check) == SQLITE_ROW) {
        printf("  %s\n", sqlite3_column_text(real_check, 0));
    }
    sqlite3_finalize(real_check);

    sqlite3_close(db);
    return corrupt > 0 ? 1 : 0;
}
