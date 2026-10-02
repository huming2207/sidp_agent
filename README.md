# Soul Agent

Rust host-side SIDP implementation for Soul Injector, with a checked SIDP v1
codec, USB CDC session, stop snapshot cache and local GDB RSP server.
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

## Compiling programming jobs

The firmware does not parse YAML. Compile the probe-rs style `target.yaml`
and the optional pre/post procedure files into a job, then copy it to the
device as `/data/job.pb`:

```sh
cargo run -- compile --target target.yaml --pre pre_prog.yaml --post post_prog.yaml -o job.pb
```

Use `--variant NAME` when `target.yaml` lists several variants. The compiler
applies the checks the firmware parser used to make and the device's fixed
size limits, so errors are reported on the host. `proto/job.proto` and
`proto/job.options` are copies of Soul Injector's `sidp_client/proto`; CI
checks they stay identical.

## CLI and GDB

```sh
cargo run -- ports
cargo run -- gdb --port /dev/ttyACM0
# In another terminal (ARM-capable GDB):
arm-none-eabi-gdb firmware.elf
(gdb) target remote 127.0.0.1:3333
(gdb) info registers
(gdb) x/16wx $sp
(gdb) hbreak main
(gdb) continue
(gdb) stepi
(gdb) detach
```

Windows ports use names such as `COM3`; macOS commonly uses `/dev/cu.usbmodem*`.
The device must expose the SIDP CDC interface and already have the firmware
backend/owner task integrated. The standalone SIDP component is not sufficient.

Only loopback listeners are accepted. One CLI process serves one GDB connection,
then exits. `--resume-on-detach` opts into resume on a clean GDB detach; otherwise
target cleanup keeps it halted. GDB EOF, process exit, or USB errors close the
physical connection. The device's disconnect policy is responsible for cleanup.
`--no-dtr` is for PTY testing only. See `gdb --help` for timeout/settling controls.

Use **`hbreak`**, not `break`: this milestone implements explicit hardware
breakpoints and rejects software breakpoint requests. GDB `load`/Flash writes,
MMIO, watchpoints, signal delivery, reset commands, RTOS threads and extended
remote launch are not supported. Native single stepping is exposed only when
advertised by the firmware. M4 FPU register descriptions expose GDB D0-D15 by
combining SIDP S-register pairs; unavailable registers remain unavailable.

## End-to-end testing without hardware

On Linux/POSIX, install an ARM-capable GDB (`gdb-multiarch` on Ubuntu), a GNU
C++20 compiler and Python 3, then run:

```sh
python3 tests/e2e/run.py \
  --sidp-client ../soul-injector/components/sidp_client \
  --gdb gdb-multiarch
```

The runner builds the **actual** C++ `sidp_session.cpp` and CRC implementation
from that checkout, links a deterministic register/RAM backend, and connects
it to the actual Rust CLI through a serial PTY. Real GDB exercises M0, M3, M4
without FPU, and M4 with FPU. The test fragments stream writes into seven-byte
pieces and covers attach, snapshot registers/stack, FPU aliases, hardware
breakpoints, continue, single step, Ctrl-C halt, register writes, multi-chunk
RAM transfers, Flash-write rejection and detach. Additional cases verify that
USB loss, invalid CRC and debugger EOF end the process without replay.

Supply `--output /tmp/sidp-e2e` to retain diagnostic logs in a chosen directory.
The runner fails rather than silently skips if GDB or the C++ component is
missing. `cargo test` needs neither of those tools nor any hardware.

GitHub Actions runs formatting, Clippy (warnings denied), tests and rustdoc on
Linux, macOS and Windows, plus the POSIX GDB end-to-end suite on Linux. CI pins
its external C++ fixture revision; update it deliberately when the protocol
changes. Physical DTR signaling, USB detach timing, SWD and instruction execution
are **not** simulated by the PTY/register model and still need hardware tests.

## License

This project is licensed under the [PolyForm Noncommercial License 1.0.0](LICENSE.md).
Commercial use requires a separate written commercial license from the copyright holder.
