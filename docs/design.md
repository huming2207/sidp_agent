# Implementation boundaries

This is one Rust package, a CLI and a reusable library. No UI, daemon framework,
plugin registry, probe-rs fork or speculative prefetch engine is involved.

- `codec`: explicit LE framing, CRC, bounded incremental SLIP.
- `transport`: ordered complete-frame trait and USB CDC implementation. A future
  WebSocket implementation will carry the same frames without SLIP and must
  preserve timeout/close/no-replay semantics. No WebSocket implementation exists.
- `session/types`: validated attach/stop/register payloads.
- `session/exchange`: request IDs, matching responses, events, timeouts and attach.
- `session/memory`, `registers`, `control`: target operations and cache discipline.
- `gdb/architecture`, `mapping`: GDB register serialization and target XML.
- `gdb/target`, `control`: RSP callbacks mapping onto the session.
- `gdb/server`: bounded-I/O event loop; USB is serviced while GDB is idle or running.

## Ownership and errors

A single owner serializes all SIDP requests. `gdbstub` callbacks are synchronous;
request handlers block for the configured deadline, while the session dispatches
interleaved SIDP events. Between requests the state-machine loop processes GDB
input and USB events, including physical loss while stopped. Reads are buffered
in bounded chunks; there is no producer queue that can accumulate speculative
operations ahead of Halt. GDB writes have a separate bounded socket timeout.

Every connection creates a fresh Session and request sequence. On a wire error,
response mismatch, request timeout or target loss the connection is closed and
snapshots discarded. Never retry writes, RUN, ATTACH or partial operations on a
new connection. DTR deassertion tells the firmware owner to begin cleanup. The
host cannot prove cleanup succeeded after losing the link.

Partial multi-chunk writes are possible: earlier chunks may have succeeded even
when a later chunk fails. Registers and stack cache are invalidated before writes.
Any target status that makes stop identity uncertain closes the session.

## Cache and memory safety

Register snapshots and stack data belong to exactly one `stop_id` in one Session.
No cache survives reconnect; RUN discards the stop, writes discard its values.
Only readable, cacheable, nonvolatile RAM can be served from the stack snapshot.
No block cache or read-ahead is implemented yet. Ordinary reads are exact and
split only at the advertised wire transfer limit. Each request must fit one
memory region. MMIO is rejected: guessing width from GDB's byte count would risk
side effects. Flash writes require the separate Soul Injector programming flow.

## GDB mapping

SIDP register IDs are not GDB remote register numbers. The XML and register byte
stream are generated from the same ordered table. R0-R15 and xPSR use the ARM
M-profile feature; M0 excludes BASEPRI/FAULTMASK. CAP_FPU adds the VFP feature,
D0-D15 (pairs of little-endian S registers) and FPSCR. GDB constructs its S aliases.
This makes a missing FPU distinct from unavailable values on a present FPU.
GDB owns symbols, expressions and unwinding. No local ELF bytes substitute for
actual target memory in this version.

## Next increments

Validate physical CDC/DTR and SWD ownership with an integrated device. Then add
latency measurement and bounded RAM read-ahead, with cache invalidation and
user-command priority tests before tuning block size. WebSocket authentication,
TLS, relay discovery and reconnect UX remain later work behind `Transport`.
