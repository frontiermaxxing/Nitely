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
You can lower retention. The database has a hard 256 MiB page cap; SQLite's temporary rollback
journal can add at most approximately another database's size (512 MiB combined budget).
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
It removes machine and attribution samples, summaries, sessions and workload metadata, retains settings, and compacts the database.
The same `NITELY_HOME` isolation used by the harness applies to telemetry; test recordings
stay in the throwaway harness data folder and no collector is detached from the harness app.
Queries return a read-only connection and never initialize the store or start recording;
only after `SQLITE_READONLY_ROLLBACK` do they briefly open the existing database writable
so SQLite can recover its hot journal, without schema/version changes, making post-crash
history available before a recorder starts.

The collection budget is under 1% of one CPU core averaged over the five-second cadence,
under 32 MiB additional resident memory for the recorder, and under 512 MiB total retained
disk including an in-flight rollback journal. On Windows, twelve collection/commit cycles
must read no more than 12 MiB and write no more than 3 MiB of process I/O: at the normal
five-second cadence, at most 205 KiB/s reads and 51 KiB/s writes. The write allowance includes
FULL-synchronous rollback-journal and database write amplification, not just sample payloads.
Run the ignored, bounded machine-only measurement alone in a build lane:
`cargo test -p nitely-core performance::tests::recorder_budget_measurement -- --ignored --exact --nocapture --test-threads=1`.
It fills maximum retention, then measures twelve native reads plus durable writes/pruning;
fixture construction, logging and cleanup are outside the process-I/O counter interval.
The test asserts SQLite FULL synchronous mode, nonzero actual writes and both transfer
budgets. Windows `GetProcessIoCounters` measures this process's transfer bytes and operation
counts; these are not physical disk traffic, flush counts or storage latency. Other I/O
operations are reported separately. Non-Windows process-I/O measurements are unavailable.
The Windows CPU assertion uses `GetProcessTimes` kernel plus user time: less than 600 ms
across twelve cycles is less than 1% of one core over their normal sixty-second sampling
window. Elapsed wall time is reported separately and must fit that sixty-second window;
FULL-sync storage waits and scheduling delays are not mislabeled as CPU consumption.
This test does not start a native application window. The database size and timings are reported rather
than inferred from the whole Rust build's peak memory.

On the development Windows machine, a full default-retention fixture occupied 70,279,168 bytes.
The I/O benchmark measured 82,112 bytes read across 44 operations and 1,129,680 bytes written
across 570 operations for twelve FULL-synchronous collection/commit cycles; other operations
transferred 26,784 bytes across 288 operations. That is about 18.4 KiB/s of process writes at
normal cadence, within the 51 KiB/s allowance. The cycles used 31.25 ms of process CPU
(about 0.052% of one core over the normal sixty-second sampling window) and took 172 ms
of elapsed wall time, with 10,665,984 bytes of additional resident memory including the
full-store fixture. An earlier run took 4.19 seconds of wall time with the same I/O volumes;
wall-time variation alone does not establish CPU consumption. Actual
overhead depends on storage and system load; these are measurements, not promised latency.

Windows API references: [PDH formatted values](https://learn.microsoft.com/en-us/windows/win32/api/pdh/nf-pdh-pdhgetformattedcountervalue),
[memory performance information](https://learn.microsoft.com/en-us/windows/win32/memory/memory-performance-information),
and [process I/O accounting](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-getprocessiocounters).

## Workload attribution

Windows collection uses native process counters at the same sampling cadence. On Windows 11
26100.4770 or newer, basic native process enumeration avoids synchronizing every process timing
counter; older Windows uses Toolhelp. Both paths read creation time and metrics through verified
process handles. The enumeration buffer is capped at 2 MiB and 8,192 process entries. A workload is
owned only when Nitely registered its launch, identified by boot GUID, PID and creation time.
Launch context can include project, issue, lane run, submit job, worker and review batch IDs.
Setup, Crew workers, reviews, lane builds, detached submits, land and install commands register
at launch. The app and background clients also register their own service process. Descendants
inherit the nearest registered root; an explicit nested root wins. Each process contributes once,
so a build beneath a worker is not added twice. A cached exact identity preserves a known child's
ownership after its parent exits; a reused parent PID cannot adopt an older child. Batched reviews
carry the batch ID, not the first issue's identity. Existing externally launched agents are not
retroactively classified from their executable names. The reserved `related` measurement remains
unknown until there is evidence for a related-but-unowned classification.

CPU percentages use process CPU deltas divided by elapsed monotonic time and the machine's
logical processor count. The first observation has no CPU or I/O delta. Private commit and the
sum of process working sets are distinct: working sets overlap and cannot be subtracted from
physical RAM to calculate exact other usage. Process read/write bytes are interval deltas, not
physical disk traffic. `unattributed_cpu_percent` is only a comparison with machine CPU and may
include access failures, unobserved processes and collection timing skew. Inclusive Windows Job
Object totals are separate workload metadata; they can capture short-lived descendants but may
overlap nested jobs and must never be added to the sampled totals.

A sample reports enumeration, measurements, denied access, missing identities, partial metrics,
first-sample status and truncation. Polling cannot observe all processes that start and exit
between polls. Missing counters stay null. The largest external contributors include only an
executable basename and exact process identity; no path, arguments, environment or prompt is
collected. Non-Windows process attribution reports unsupported. Workload registration is best
effort, capped at 20 ms of database lock waiting, and cannot prevent a command from launching.

Attribution follows the same 24-hour detailed and 30-day minute retention settings. Detailed rows
are capped at 17,280 and minute rows at 43,200. Each stored observation is at most 1,792 bytes;
compact tuples reference a dictionary of at most 8,192 workload records, each at most 2,048 bytes.
The most significant workloads and external processes fit first; dropped breakdown entries set
truncation while the aggregate totals remain. Minute rows retain sampled CPU/memory peaks and
sum observed I/O deltas, with their observation count and `minute_peak` resolution. Workload
metadata remains while retained observations refer to it. Reaching the dictionary cap makes
coverage partial; it does not silently evict metadata still explaining historical observations.
The database cap applies to machine history, attribution, indexes and metadata together.

Queries are read-only, newest insertion first, and capped at 1,000 observations. Project filters
limit only workload breakdowns and metadata; machine-wide totals and external processes retain
their machine-wide meaning. `attribution_bounds` exposes the retained range for coverage reporting.
`recent_workloads` includes registered work that exited entirely between polls. An observed exit
time is explicitly distinguished from a launcher's recorded completion time. Recording uses two
bounded transactions per interval: machine sample/minute, then attribution sample/minute. A
failure between them can leave a machine sample without attribution; consumers must show that gap.

Run `cargo test -p nitely-core attribution -- --include-ignored --nocapture --test-threads=1`
in a build lane for synthetic identity cases, native process/short-lived Job tests and the combined
full-retention CPU, memory and storage measurement. The combined test excludes compilation and
fills retained storage before timing twelve native collection/write cycles. CPU is measured using
Windows process CPU time, separately from wall-clock I/O waiting; memory is additional sampled
resident usage during the collection/write cycles.


On the development Windows machine, the combined machine/attribution full-retention fixture
occupied 230,072,320 bytes. Twelve native collection and durable write cycles took 742 ms wall
time and 250 ms process CPU, equivalent to 0.42% of one core over twelve five-second intervals.
Additional sampled resident usage was 2,879,488 bytes. A native process snapshot covering 855
accessible processes (181 denied) took 17.7 ms; the 10,000-process synthetic attribution fixture
took 47 ms. These are measured debug-build results on that machine, not guaranteed latency;
older Windows uses the slower Toolhelp fallback. The combined budget test enforces under 1%
of one core and under 32 MiB additional sampled resident usage.
