# Schema migrations

Every change to Nitely's database schema is one file here, named
`<yyyymmdd>-<what>.sql` in lower-case letters, digits and dashes, for example
`20261020-issue-size.sql`:

```sql
ALTER TABLE issue ADD COLUMN size INTEGER NOT NULL DEFAULT 0;
```

`build.rs` lists the files in name order and `db.rs` runs each one a database has not run yet,
inside the upgrade's transaction, recording its name in `schema_migration`. A new database gets
`SCHEMA` in `db.rs` (frozen at schema 22) and then every file here, so the file is the only place
the change goes: do not also add the column or table to `SCHEMA`.

Two branches that each add a file touch no common lines, so they rebase without a conflict.

Never edit, rename or delete a file that has landed: databases have run it under that name, and a
database holding a name a build does not have is one a newer Nitely upgraded, which that build
refuses to open.
