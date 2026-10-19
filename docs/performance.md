# Machine performance history

Nitely records machine usage across projects in `<Nitely data>/performance/history.sqlite`,
separate from the issue database. The desktop, MCP server, Crew supervisor, remote server,
and active CLI build/submit/land processes participate. One OS file lock elects a recorder;
other clients wait and take over if it exits. Closing a page has no effect. Closing every
participating client stops recording; `nitely performance record` keeps it running without
the desktop. This creates no login item or extra subprocess. A new recorder generation asks
older collectors to relinquish ownership within a sampling interval.

Samples arrive about every five seconds while a client is running. Native Windows counters
cover CPU, physical total/available RAM, system commit/limit, paged and nonpaged kernel pools,
pages read/written per second, and physical disk read/write throughput, transfer latency and
queue. PDH's English counter names work on localized Windows. Failed/unsupported counters,
including rate counters before their second reading, are `null`, never zero. Other platforms
currently provide physical RAM only. Recording reads no command arguments, environment
contents, file contents, URLs or agent prompts. Physical available RAM already includes
reclaimable standby memory; it is not added again. Kernel paged bytes are allocation, not
necessarily resident RAM. Paging and disk activity are observations, not proof of a leak.

Every sample has UTC wall time, monotonic time, collector session, and on Windows the native
boot GUID. Windows monotonic time is boot-relative and includes sleep; other platforms use
collector-relative monotonic time and have no boot identity yet. A clean client exit or pause
marks its session stopped. An unmarked end is **unexplained**, not automatically a crash.
A changed known boot GUID establishes a restart. Readers must show missing intervals and
clock discontinuities rather than joining them into a continuous chart. A wall-clock change
greater than two seconds relative to monotonic time starts a new minute summary.

Default retention is 24 hours of detailed samples and 30 days of minute summaries. Summaries
retain the last observed values, per-metric maxima, minimum available memory and sample count;
maxima are not averages. Sessions never share a summary. Retention uses wall-time expiry plus
independent row caps (17,280 detailed, 43,200 summaries, 4,096 sessions), so turning the clock
back cannot grow the store indefinitely. Turning it forward may expire history earlier.
You can lower retention. The database has a hard 128 MiB page cap; SQLite's temporary rollback
journal can add at most approximately another database's size (256 MiB combined budget).
Full/corrupt/unwritable storage stops that collection attempt and retries after five seconds;
it never blocks task execution. One bounded diagnostic is kept in `last-error.txt`. A full
disk can prevent even the diagnostic; readers must still display stale history as stale.

Each sample and its updated minute summary commit together with SQLite FULL synchronous
rollback-journal durability. An interrupted transaction is recovered on reopen; complete prior
transactions remain. The maximum normal unrecorded interval is one five-second cadence plus
the current collection/commit time. OS suspension, storage failure or forced termination can
make that gap longer. Invalid JSON rows are skipped without discarding other records; structural
database damage is reported instead of silently replacing the history. There is no crash promise
about storage hardware that does not honor flushes.

```
nitely performance status
nitely performance record --seconds 60
nitely performance configure --detailed-hours 12 --summary-days 7
nitely performance configure --enabled false
nitely performance clear
nitely performance configure --enabled true
```

Status/history reads never start a collector. Pause takes effect at the next sample. Clear
requires pause and ownership of the recorder lock; if it is stopping, retry after five seconds.
It removes samples, summaries and sessions, retains settings, and compacts the database.
The same `NITELY_HOME` isolation used by the harness applies to telemetry; test recordings
stay in the throwaway harness data folder and no collector is detached from the harness app.

The collection budget is under 1% of one CPU core averaged over the five-second cadence,
under 32 MiB additional resident memory for the recorder, and under 256 MiB total retained
disk including an in-flight rollback journal. Run the ignored, bounded measurement in a build
lane: `cargo test -p nitely-core performance::tests::recorder_budget_measurement -- --ignored --nocapture`.
It fills maximum retention and measures native reads plus durable writes/pruning. This test
does not start a native application window. The database size and timings are reported rather
than inferred from the whole Rust build's peak memory.

On the development Windows machine, a full default-retention fixture occupied 70,279,168 bytes.
Twelve collections plus FULL-synchronous writes/pruning took 353 ms (about 29 ms each, under
0.6% of one core's five-second interval using wall time as a conservative bound). Actual
overhead depends on storage and system load; these are measurements, not promised latency.

Windows API references: [PDH formatted values](https://learn.microsoft.com/en-us/windows/win32/api/pdh/nf-pdh-pdhgetformattedcountervalue)
and [memory performance information](https://learn.microsoft.com/en-us/windows/win32/memory/memory-performance-information).
