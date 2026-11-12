# Soul Agent desktop app

A desktop app for setting up Soul Injector debugging and programming without
knowing how the Soul Injector or SIDP work. It is built with Longbridge's
[GPUI Kit](https://gpui-kit.com) and uses the `sidp-agent` library for
everything that talks to the device, so it behaves exactly like the
command-line tool.

Linux and macOS are supported. Windows is planned; see
[Adding Windows](#adding-windows).

## Building

The app is a separate Cargo workspace with its own `Cargo.lock`, so its
dependencies never change the versions `sidp-agent` is built and tested with.
Build it from this folder:

```sh
cd desktop
cargo run --release
```

Linux needs the development files GPUI links against. On Debian and Ubuntu:

```sh
sudo apt install libxkbcommon-dev libxkbcommon-x11-dev libxcb1-dev \
    libfreetype-dev libfontconfig-dev libvulkan1 mesa-vulkan-drivers
```

A missing `libxkbcommon-x11-dev` shows up as
`unable to find library -lxkbcommon-x11` when linking. macOS needs Xcode or
its command-line tools.

## Using it

The Soul Injector and your saved profiles are on the left. Across the top,
three cards choose what you want to do. Each has its own page, and a dot on
a card shows that something is running there.

**Program** puts firmware on targets, which is what the Soul Injector was
first built for.

1. *Target*: the `target.yaml` and the firmware image (`.bin`); for ESP32
   targets, one file per image. The app checks the files as you go and says
   what is missing. Pre/post-program procedures and the job name are under
   *Production options*.
2. *Program*: program now, or only send the job. The switch makes the Soul
   Injector program every target plugged into it, without this computer.

**Debug** turns the Soul Injector into a GDB server, like `sidp-agent debug`
and `sidp-agent gdb`.

1. *Target*: as above, plus the ELF with debug symbols.
2. *Debug server*: *Program, then debug* programs the target and stops it at
   its first instruction; *Debug only* stops it wherever it is. *Measure
   power during the session* adds a compact power view that starts and stops
   with the debug server. The options match the CLI's: connect under reset,
   resume on detach, and keep serving after GDB disconnects (for IDEs that
   restart debugging; in *Program, then debug* each new session starts from
   reset without programming again). *Stop debugging* leaves the target as a
   GDB detach would. A GDB `kill`, or GDB leaving without detaching, keeps it
   halted.
3. *Connect your debugger*: the GDB command line and settings for VS Code
   (Microsoft C/C++ and Cortex-Debug) and CLion, with paths and port filled
   in. ARM GDBs are found in `PATH` and the usual toolchain folders.

ESP32 targets cannot be debugged through the Soul Injector; the Debug page
says so and offers to program them instead.

**Power** measures the target's current over time, in the style of Nordic's
Power Profiler. The Soul Injector does not supply or switch target power:
feed VPP input (connector pin 11) from 3.3 V (pin 7) or a 1.8 to 3.6 V bench
supply, and power the target from VPP output (pin 9).

- Choose the sample rate and how long to record, then *Start*. The rates are
  the Soul Injector's: 227 and 909 samples per second, and 1.7 kS/s, the
  fastest it reads. Frames damaged on the USB link are dropped and show as
  gaps (see the open items in Soul Injector's `docs/en/rev8-hardware.md`).
- The chart shows current on a log or linear scale. As in the Power
  Profiler, the mouse wheel zooms time around the pointer, dragging pans
  (and stops following new samples; *Back to newest* resumes), and sideways
  scrolling pans too. The buttons jump to windows from 10 ms to the whole
  recording. The strip under the chart shows which current range (LOW, MID,
  HIGH) each part was measured in. The charts fill the window: the current
  chart takes 45% of its height.
- A second chart shows the supply (`VPP_INPUT`) over the same window, on a
  linear scale fitted to the measured voltages (at least 50 mV tall) so a sag
  stands out. Hovering shows time, current, voltage and range.
- Shift-drag across the chart to select part of it. *Window* and *Selection*
  show average, maximum and minimum current, charge, energy, duration, and
  average, maximum and minimum voltage.
- *Export CSV* saves the selection, or everything, as
  `time_s,current_a,supply_v,range`.

*Measure from: Soul Injector* records over the same USB connection as
everything else, so while it records the Soul Injector is busy: to measure
during debugging, turn on *Measure power during the session* on the Debug page
instead. The current is corrected by the zero offset: with nothing connected
to `VPP_OUTPUT`, *Zero offset* measures what each range reads on its own
(mostly the sensors' input bias) and the app subtracts it from later
recordings of that Soul Injector. Samples are placed by the device's clock, so
time and charge stay right even when a conversion is missed. *Simulated
preview* generates a low-power MCU's waveform (a few µA asleep, wake-ups and
radio bursts) so the page can be tried without a device; it is labelled as
simulated wherever it is shown.

The left column shows the device's serial number, firmware, storage, job and
last result, and pulls the production log (appended as JSON Lines to the file
chosen in Settings). The Soul Injector is found by its USB product name, and
plugging or unplugging it is noticed within two seconds.

Settings (time server, timeouts, log file) are under the app menu on macOS
(⌘,) and at the bottom of the sidebar on Linux (Ctrl+,). Profiles, settings
and the last page used are saved in `~/.config/soul-agent/state.json` on Linux
and `~/Library/Application Support/Soul Agent/state.json` on macOS.

### Troubleshooting

- **Permission denied on Linux**: add yourself to the `dialout` group
  (`sudo usermod -aG dialout $USER`), then log out and back in.
- **Port in use**: another program has the Soul Injector's port open, such as
  a running `sidp-agent` or a serial terminal. Only one program can use it at
  a time.
- **GDB not found**: install the
  [Arm GNU Toolchain](https://developer.arm.com/downloads/-/arm-gnu-toolchain-downloads)
  or `gdb-multiarch`, then choose *Search again* in Settings, or pick the
  executable with *Browse…*.

## How it is built

Everything except `src/ui` is plain Rust, independent of GPUI and tested
without a display:

| Module | Purpose |
| --- | --- |
| `profile` | Saved profiles and settings, written atomically as JSON. |
| `target` | Summarizes `target.yaml` and builds jobs with `sidp_agent::compile`, the CLI's compiler. |
| `ports` | Finds USB serial ports and recognizes Soul Injectors. |
| `toolchain` | Finds ARM-capable GDB executables. |
| `snippets` | GDB, VS Code and CLion settings for the running server. |
| `worker` | Runs one device operation at a time on a background thread and reports events. |
| `worker::bridge` | Puts `sidp_agent::gdb::serve` behind a loopback relay so the app can see GDB connect and can stop a session. |
| `worker::power` | Records power on the worker's connection, alone or during a debug session, and measures the zero offset. |
| `power` | Power recordings: statistics, chart envelopes, CSV export, the simulated preview, and placing device samples on the recording's time grid. |
| `describe` | Plain-language text for device statuses and run results. |
| `platform` | Everything that differs between operating systems. |
| `ui` | The GPUI Kit views. `ui::app` holds state and behavior; the rest only draws. |

Each operation opens its own USB connection, as one CLI command does. Uploads
use the same requests as `sidp_agent::manage::push_job`, with progress and
cancellation added. Power data arrives on the debug connection, through the
session's power sink.

### Adding Windows

OS-specific code lives in `src/platform.rs`. Settings folders, executable
lookup (`.exe`) and command quoting already have Windows branches.
`open_in_terminal` returns *not supported*, and the UI hides that button on
Windows. The serial port list already handles `COM` ports. What remains is
testing the build and USB CDC behavior on Windows, and a terminal launcher.

## Tests

```sh
cargo test
```

`tests/ui.rs` drives the real window headless with GPUI Kit's test support:
it clicks buttons, types into fields and picks variants with the keyboard, then
checks the result: for example that each mode shows only its own page, that a
filled-in target becomes a ready job and is saved, that deleting a profile
asks first, that an invalid GDB port is not used, and that the simulated power
preview records until stopped. These tests use a temporary state file, never
your settings, and never open a Soul Injector plugged into the computer.

The debug-session tests drive the worker against Soul Injector's real C++
SIDP session (the same mock endpoint as the CLI's end-to-end tests), with the
test acting as GDB. Build the endpoint and run them with:

```sh
C=../../soul-injector/components/sidp_client
c++ -std=gnu++20 -I$C/include -I$C/test/host_stubs ../tests/e2e/mock_endpoint.cpp \
    $C/sidp_session.cpp $C/sidp_transport.cpp -o /tmp/mock-endpoint
SIDP_MOCK_ENDPOINT=/tmp/mock-endpoint cargo test -- --include-ignored
```

They cover a clean detach, attaching again for the next GDB session, GDB
leaving without detaching, stopping the server with and without GDB
connected, and debugging on when the device cannot measure power.

`tests/hardware.rs` records power from a real Soul Injector with a profiler
(Rev 8), alone and during a debug session:

```sh
SIDP_DEVICE=/dev/ttyACM0 cargo test --test hardware -- --ignored --test-threads 1
```

## License

This project is licensed under the [PolyForm Noncommercial License 1.0.0](../LICENSE.md).
Commercial use requires a separate written commercial license from the copyright holder.
