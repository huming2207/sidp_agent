# Soul Agent

Rust host-side SIDP implementation for Soul Injector. Development starts with
SIDP v1 framing and proceeds to a USB CDC session and a local GDB RSP server.
WebSocket and graphical UI support are outside this first milestone.

## Development

```sh
cargo test
cargo doc --no-deps --open
```

The authoritative wire contract is Soul Injector's
`components/sidp_client/include/sidp_defs.hpp` and `docs/sidp-protocol.md`.
This implementation uses explicit little-endian fields, a maximum 8192-byte
frame, CRC-32/ISO-HDLC, and bounded SLIP decoding. No native structure layout
or unsafe casting is used.

## Session contract

`session::Session<T>` owns one `transport::Transport` and serializes requests.
USB CDC is implemented by `transport::UsbCdc`; future WebSocket code can implement
that message-level trait without changing the session or cache rules.

Attach supports Cortex-M0/M3/M4 (M4 FPU is explicitly capability-gated). The
session requires a confirmed initial stop, preserves unavailable registers,
and optionally serves cacheable RAM from the current stack snapshot. Writes
invalidate snapshots; run and disconnect discard stop state. There is no
speculative prefetch, ordinary memory-block cache, automatic reconnect or replay.
Reads/writes are split at the firmware's advertised limit. MMIO is rejected by
this initial byte-oriented interface, and Flash writes are never supported.

USB open toggles DTR, clears stale driver buffers and allows a configurable
settling interval before ATTACH. An attach timeout means close/reopen manually;
there is no readiness handshake in SIDP v1. DTR deassertion on close triggers
the firmware's target cleanup. Host tests cannot validate physical USB timing.

## License

This project is licensed under the [PolyForm Noncommercial License 1.0.0](LICENSE.md).
Commercial use requires a separate written commercial license from the copyright holder.
