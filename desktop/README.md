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

The window has the Soul Injector and your profiles on the left, and four steps
on the right:

1. **Target**: the `target.yaml`, the firmware image (`.bin`) and the ELF with
   debug symbols. The app checks the files as you go and says what is missing.
   Pre/post-program procedures and the job name are under
   *Production options*.
2. **Debug server**: *Program, then debug* sends the job, programs the target
   and halts it at its first instruction, like `sidp-agent debug`.
   *Attach to running target* is like `sidp-agent gdb`. *Connect under reset*
   and *Let the target run when GDB detaches* match the CLI options.
   *Keep serving after GDB disconnects* waits for the next GDB session, for
   example when an IDE restarts debugging; in *Program, then debug* mode each
   new session starts from reset without programming again.
3. **Connect your debugger**: the GDB command line and ready-made settings for
   VS Code (Microsoft C/C++ and Cortex-Debug) and CLion, with the paths and
   port filled in. ARM GDBs are found in `PATH` and the usual toolchain
   folders. On Linux and macOS, *Open in terminal* starts GDB already connected.
4. **Programming**: program without debugging, or only send the job. The
   switch makes the Soul Injector program every target plugged into it.

ESP32 targets cannot be debugged through the Soul Injector, so for them only
the target and programming steps are shown.

The left column shows the device's serial number, firmware, storage, job and
last result, and pulls the production log (appended as JSON Lines to the file
chosen in Settings). The Soul Injector is found by its USB product name, and
plugging or unplugging it is noticed within two seconds.

Settings (time server, timeouts, log file) are under the app menu on macOS
(⌘,) and at the bottom of the sidebar on Linux (Ctrl+,). Profiles and settings
are saved in `~/.config/soul-agent/state.json` on Linux and
`~/Library/Application Support/Soul Agent/state.json` on macOS.

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
| `describe` | Plain-language text for device statuses and run results. |
| `platform` | Everything that differs between operating systems. |
| `ui` | The GPUI Kit views. `ui::app` holds state and behavior; the rest only draws. |

Each operation opens its own USB connection, as one CLI command does. Uploads
use the same requests as `sidp_agent::manage::push_job`, with progress and
cancellation added. Nothing in `sidp-agent` was changed for the app.

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
checks the result, for example that a filled-in target becomes a ready job and
is saved, that deleting a profile asks first, and that an invalid GDB port is
not used. These tests use a temporary state file, never your settings.

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
leaving without detaching, and stopping the server with and without GDB
connected.

## License

This project is licensed under the [PolyForm Noncommercial License 1.0.0](../LICENSE.md).
Commercial use requires a separate written commercial license from the copyright holder.
