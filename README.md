# Soul Agent

Rust host-side SIDP implementation for Soul Injector, with a checked SIDP v1
codec, USB CDC session, stop snapshot cache and local GDB RSP server, plus the
Soul Injector management commands: compiling and pushing programming jobs,
running them, and collecting the production log.
WebSocket and graphical UI support are outside this first milestone.

## Development

```sh
cargo test
cargo doc --no-deps --open
```

The authoritative wire contract is Soul Injector's
`components/sidp_client/include/sidp_defs.hpp` and the schemas in
`components/sidp_client/proto`.
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

## Programming jobs

The firmware does not parse YAML and keeps exactly one job. Push it over USB
with its images; the agent compiles the YAML, uploads only what the device
does not already have, and activates the job:

```sh
cargo run -- job push --port /dev/ttyACM0 \
    --target target.yaml --pre pre_prog.yaml --post post_prog.yaml \
    --image firmware.bin=build/app.bin [--name line-a] [--auto]
cargo run -- job run --port /dev/ttyACM0 --wait    # one run, waits for the result
cargo run -- job status --port /dev/ttyACM0
cargo run -- job cancel --port /dev/ttyACM0
cargo run -- device info --port /dev/ttyACM0
cargo run -- asset push --port /dev/ttyACM0 build/app.bin --name firmware.bin
```

`--image NAME=FILE` gives each image the job programs: `firmware.bin` for
Cortex-M targets, and for ESP32 targets every `images[].path` in
`target.yaml`, which must be `/data/<NAME>`. The job pins each image's SHA-256
and the device refuses to run with any other image. `--auto` also runs the job
whenever a target is plugged in; otherwise it runs only on `job run`. A device
busy programming answers `STATUS_BUSY` to anything that would change the job
or its files.

`compile` writes the same job to a file without a device (`-o job.pb`). Use
`--variant NAME` when `target.yaml` lists several variants. The compiler
applies the checks the firmware parser used to make and the device's fixed
size limits, so errors are reported on the host.

## Production log

The device records every run and every boot until the log is collected:

```sh
cargo run -- log pull --port /dev/ttyACM0 --output production.jsonl [--no-ack]
```

New entries are appended as JSON Lines (`device`, `id`, `boot`, `uptime_us`,
`utc_ms`, `type`, and for runs `run_id`, `outcome`, `failed_stage`,
`duration_ms`, `trigger`, `job_name`, `job_sha256`, `error_code`), synced to
disk, and only then acknowledged so the device may reuse the space. `log pull`,
`job push` and `job run` first set the device clock, which gives entries their
`utc_ms`. The time comes from an NTP server (`--ntp-server`, default
`pool.ntp.org`), not from this computer's clock, so the device needs no network
of its own. Without NTP the clock is left unset and a warning is printed. All
times are UTC; convert to local time only when displaying them. A device whose log is full refuses runs with `STATUS_LOG_FULL` (30)
until the log is pulled.

`proto/*.proto` and `proto/*.options` are copies of Soul Injector's
`sidp_client/proto`; CI checks they stay identical. The management opcodes are
described in `proto/manage.proto`.

## Power measurement

Boards with the power profiler (Rev 8) measure the target's current and
`VPP_INPUT` while it runs, programs or is debugged:

```sh
cargo run -- power info --port /dev/ttyACM0
cargo run -- power zero --port /dev/ttyACM0 --output zero.json   # nothing on VPP_OUTPUT
cargo run -- power record --port /dev/ttyACM0 --seconds 10 --output power.csv \
    [--zero zero.json] [--conversion-us 1100] [--averages 1] [--lowest-range low]
```

`record` writes `time_s,current_a,vin_v,vout_v,range,settling,gap` and prints
the average, minimum and maximum. The sample period is the conversion time
times the averages; `power info` lists the accepted values and the shortest
period. `vout_v` is computed from `vin_v` and the range's series resistance,
because the Rev 8 sensors only measure `VPP_INPUT`. `gap` marks samples after
time that was not measured (a range change, a missed conversion or a dropped
frame); `settling` marks samples just after a step down, which include output
capacitor recharge.

The device sends raw INA226 readings (`power` module); conversion and the zero
correction happen here. `power zero` holds each range with nothing connected to
`VPP_OUTPUT` and stores the open-output current per volt, mostly the INA226
input bias, to subtract later. The power service is described in
`sidp_client/docs/sidp-power.md`. Power data shares the debug connection, so
one program owns both: the CLI records power only while it is not serving GDB.

A frame that fails its CRC, size or version check is dropped rather than
closing the connection, as SIDP requires; a lost response then ends in its
request's timeout, and lost power data shows as a gap. `power record` reports
how many were dropped ("damaged on the wire"). Rev 8 sends roughly one damaged
frame per 1400 at 588 µs; the device-side cause is not found yet.

## WiFi (preview)

Rev 8 can serve SIDP over WiFi. The Soul Injector connects to this computer,
so set it up over USB, restart it, and keep `sidp-agent serve` (or the desktop
app) running to hold it. Other commands then use it by its device ID:

```sh
cargo run -- network set --port /dev/ttyACM0 --ssid OfficeWiFi   # asks for both passwords
cargo run -- network show --port /dev/ttyACM0                    # shows the device ID
cargo run -- serve                                               # listens on 0.0.0.0:80
cargo run -- gdb --port ws://127.0.0.1/7CDFA1B2C3D4              # in another terminal
```

The device ID is the Soul Injector's factory WiFi MAC address, 12 hex digits
(`7c:df:a1:b2:c3:d4` is accepted too). `serve` lists devices as they connect
and forwards each command's connection to its device unchanged, login
included; it accepts commands only from this computer, and one at a time per
device (a second is told the device is in use). It drops a device that has
been silent for 30 s (devices ping every 10 s). Without `serve`,
`--port ws://0.0.0.0` makes a single command wait for the device itself.

`network set` asks for the WiFi password and for a device password, which is
needed to use the Soul Injector over the network. Only an SRP salt and
verifier made from it are stored on the device. Over WiFi every command logs
in first, with `--password`, `SIDP_PASSWORD`, or a prompt; the password never
crosses the network, and the device proves it knows it too (`login` module:
the SRP6a handshake of ESP-IDF's protocomm Security 2).

The device connects to `ws://HOSTNAME.local/` by default: this computer's
mDNS name, so it follows the computer when its address changes and never
reaches a colleague's agent. Linux (Avahi) and macOS answer for that name;
`--server ws://ADDRESS[:PORT]/` sets a fixed address instead. The port, 80
unless given (443 is planned for TLS), is stored on the device as part of that
URL, and `serve --listen 0.0.0.0:PORT` must use the same one. The firewall must
accept that TCP port. On Linux, ports below 1024 need
`sudo sysctl net.ipv4.ip_unprivileged_port_start=80` (or
`sudo setcap cap_net_bind_service=+ep` on the binary), or a port above 1023;
the agent says so when it cannot listen. A dropped device tries again within
3 s.
The device stays connected between SIDP connections; text messages do what
DTR does on USB (`open`, `ready` or `busy`, `close`, `closed`). USB has
priority: a USB request ends a WiFi connection, and the WiFi user is told.
Settings are stored in the device's NVS (namespace `si.cfg`) and can only be
changed over USB; `network off` turns WiFi off from the next restart.

This follows `docs/en/network-debugging-plan.md` in the Soul Injector
repository. Traffic after the login is not encrypted, so use it only on a
network where nobody can read or alter traffic.

## CLI and GDB

```sh
cargo run -- ports
# Program the target, halt it at reset and serve GDB:
cargo run -- debug --port /dev/ttyACM0 --target target.yaml \
    --image firmware.bin=build/app.bin
# Or attach to whatever the target is already running:
cargo run -- gdb --port /dev/ttyACM0
# In another terminal (ARM-capable GDB):
arm-none-eabi-gdb firmware.elf
(gdb) target remote 127.0.0.1:3333
(gdb) info registers
(gdb) x/16wx $sp
(gdb) break main
(gdb) continue
(gdb) next
(gdb) monitor reset
(gdb) detach
```

`debug` takes the same job options as `job push` and fails before attaching if
the run does not pass. It sets the job's trigger to manual, so use `job push
--auto` afterwards to re-arm a job that runs when a target is plugged in. Both
`debug` and `gdb` accept `--connect-under-reset`, which holds the target in
reset while connecting and stops it before its first instruction. Use it for
firmware that sleeps or reuses the SWD pins soon after boot.

`monitor reset` uses a system reset and `monitor reset nrst` pulses the reset
pin. Both leave the target halted at its reset vector. GDB does not re-read
registers after a monitor command, so run `maint flush register-cache` to see
them; `continue` and `stepi` are unaffected.

After each stop GDB reads the same few bytes of flash around the PC dozens of
times. With the flash cache on, the agent reads each aligned 64-byte block of
flash once and answers those reads from it until the target runs again (any
continue, step or reset ends it). A halted core cannot change its flash, so
the cache is safe unless something else writes flash while this core is
halted: another core, or a flash operation started just before the halt on a
part that does not stall reads during it. It is on by default over WiFi, where
it makes `stepi` about 10× faster, and off over USB. `--flash-cache on|off` on
`gdb` and `debug` overrides the default, and `monitor flash-cache [on|off]`
shows or changes it during a session.

Windows ports use names such as `COM3`; macOS commonly uses `/dev/cu.usbmodem*`.
The device must expose the SIDP CDC interface and already have the firmware
backend/owner task integrated. The standalone SIDP component is not sufficient.

Only loopback listeners are accepted. One CLI process serves one GDB connection,
then exits. `--resume-on-detach` opts into resume on a clean GDB detach; otherwise
target cleanup keeps it halted. GDB EOF, process exit, or USB errors close the
physical connection. The device's disconnect policy is responsible for cleanup.
`--no-dtr` is for PTY testing only. See `gdb --help` for timeout/settling controls.

The agent gives GDB the target's memory map with Flash marked read-only, so
GDB uses hardware breakpoints there for `break`, `next` and `finish`. Software
breakpoints are rejected, so a plain `break` in RAM fails. Memory outside the
map cannot be read. GDB `load`/Flash writes, MMIO, watchpoints, signal delivery,
RTOS threads and extended remote launch are not supported. Native single
stepping is exposed only when advertised by the firmware. M4 FPU register
descriptions expose GDB D0-D15 by combining SIDP S-register pairs; unavailable
registers remain unavailable.

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
breakpoints, the memory map, `monitor reset`, continue, single step, Ctrl-C
halt, register writes, multi-chunk RAM transfers, Flash-write rejection and
detach. Additional cases verify that USB loss, invalid CRC and debugger EOF end
the process without replay.

Supply `--output /tmp/sidp-e2e` to retain diagnostic logs in a chosen directory.

The login is checked against ESP-IDF's own Security 2 handler, built for this
computer:

```sh
tests/e2e/srp_device/build.sh /tmp/srp     # needs IDF_PATH; Python with jsonschema, jinja2
SIDP_SRP_DEVICE=/tmp/srp/srp-device cargo test --release login -- --include-ignored
```
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
