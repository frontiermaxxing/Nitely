"""CI smoke: preserve old rows across migration, refuse future schema, restore known-good data."""
import pathlib
import sqlite3
import subprocess
import sys


def check(cli, database):
    database = pathlib.Path(database)
    backup = database.with_suffix(".recovery")
    with sqlite3.connect(database) as conn:
        if conn.execute("SELECT count(*) FROM event WHERE summary = 'release-upgrade-preserve-me'").fetchone()[0] != 1:
            raise RuntimeError("upgrade lost baseline data")
        if conn.execute("PRAGMA integrity_check").fetchone()[0] != "ok":
            raise RuntimeError("migrated database is corrupt")
        with sqlite3.connect(backup) as saved:
            conn.backup(saved)
        conn.execute("INSERT INTO schema_migration(name, applied_at, applied_by) VALUES (?, 0, ?)",
                     ("99999999-release-recovery-fixture.sql", "future-fixture"))
    refused = subprocess.run([cli, "check-database"], capture_output=True)
    if refused.returncode != 3:
        raise RuntimeError("candidate accepted a future schema")
    with sqlite3.connect(backup) as saved, sqlite3.connect(database) as conn:
        saved.backup(conn)
    subprocess.run([cli, "check-database"], check=True)
    subprocess.run([cli, "status", "release-recovery-restored", "--session", "release-recovery"], check=True)
    with sqlite3.connect(database) as conn:
        if conn.execute("SELECT count(*) FROM event WHERE summary = 'release-upgrade-preserve-me'").fetchone()[0] != 1:
            raise RuntimeError("recovery lost baseline data")


if __name__ == "__main__":
    check(*sys.argv[1:])
