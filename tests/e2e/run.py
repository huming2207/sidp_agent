#!/usr/bin/env python3
"""Run real GDB -> CLI -> serial PTY -> real C++ SIDP session -> mock Cortex-M.

Linux/POSIX only. Requires cargo, C++20, Python 3, and ARM-capable GDB.
The C++ SIDP component is supplied explicitly; no firmware source is vendored.
All subprocesses have bounded lifetimes. Logs remain in --output for diagnosis.
"""
import argparse
import os
from pathlib import Path
import pty
import select
import signal
import shutil
import subprocess
import tempfile
import threading
import time
import tty
from failures import scenarios

ROOT = Path(__file__).resolve().parents[2]


def pump(source, destination, stop):
    """Forward stream fragments without assuming read boundaries are packets."""
    try:
        while not stop.is_set():
            if not select.select([source], [], [], 0.1)[0]:
                continue
            data = os.read(source, 4096)
            if not data:
                return
            # Fragment every stream deliberately, including escape sequences.
            for start in range(0, len(data), 7):
                chunk = data[start:start + 7]
                while chunk:
                    chunk = chunk[os.write(destination, chunk):]
    except OSError:
        if not stop.is_set():
            raise


def scenario(agent, endpoint, gdb, output, profile, fpu):
    """Exercise one profile with a fresh physical link and GDB process."""
    label = f"m{profile}-{'fpu' if fpu else 'basic'}"
    master, slave = pty.openpty()
    tty.setraw(slave)
    stop = threading.Event()
    processes, threads = [], []
    endpoint_log = open(output / f"{label}-endpoint.log", "wb")
    agent_log = open(output / f"{label}-agent.log", "wb")
    try:
        mock = subprocess.Popen([str(endpoint), str(profile), "fpu" if fpu else "basic"],
                                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=endpoint_log)
        processes.append(mock)
        for source, destination in [(master, mock.stdin.fileno()), (mock.stdout.fileno(), master)]:
            thread = threading.Thread(target=pump, args=(source, destination, stop), daemon=True)
            thread.start()
            threads.append(thread)
        proc = subprocess.Popen([str(agent), "gdb", "--port", os.ttyname(slave),
                                 "--listen", "127.0.0.1:0", "--no-dtr", "--settle-ms", "0"],
                                stdout=subprocess.PIPE, stderr=agent_log)
        processes.append(proc)
        if not select.select([proc.stdout], [], [], 10)[0]:
            raise AssertionError("Agent did not announce readiness")
        line = proc.stdout.readline().decode().strip()
        assert line.startswith("GDB listening on "), line
        address = line.removeprefix("GDB listening on ")
        script = output / f"{label}.gdb"
        script.write_text(f'''set pagination off
set confirm off
set remotetimeout 5
target remote {address}
python
import gdb
assert int(gdb.parse_and_eval('$pc')) == 0x08000000
assert int(gdb.parse_and_eval('$sp')) == 0x20000200
assert int(gdb.parse_and_eval('$xpsr')) == 0x01000000
assert int(gdb.parse_and_eval('$r0')) == 0
assert bytes(gdb.selected_inferior().read_memory(0x20000200, 4)) == b'\\x42\\x00\\x00\\x00'
assert int(gdb.parse_and_eval('$msp')) == 0x20000200
if {fpu!r}:
    assert float(gdb.parse_and_eval('$s0')) == 1.0
    assert float(gdb.parse_and_eval('$s1')) == 2.0
    gdb.execute('set $s0 = 3.0')
    assert float(gdb.parse_and_eval('$s0')) == 3.0
else:
    # Unknown register names can become void convenience variables in GDB.
    assert gdb.parse_and_eval('$fpscr').type.code == gdb.TYPE_CODE_VOID
if {profile} == 1:
    assert gdb.parse_and_eval('$basepri').type.code == gdb.TYPE_CODE_VOID
else:
    assert int(gdb.parse_and_eval('$basepri')) == 0
# Explicit hardware breakpoint; software patching is unsupported.
gdb.execute('hbreak *0x08000010')
gdb.execute('continue')
assert int(gdb.parse_and_eval('$pc')) == 0x08000010
gdb.execute('delete breakpoints')
gdb.execute('stepi')
assert int(gdb.parse_and_eval('$pc')) == 0x08000012
# The host runner sends SIGINT to GDB during this unbounded continue.
gdb.execute('continue')
assert int(gdb.parse_and_eval('$pc')) == 0x08000012
gdb.execute('set $r0 = 123')
assert int(gdb.parse_and_eval('$r0')) == 123
assert bytes(gdb.selected_inferior().read_memory(0x20000100, 4)) == b'\\x78\\x56\\x34\\x12'
data = bytes(range(150))
gdb.selected_inferior().write_memory(0x20000300, data)
assert bytes(gdb.selected_inferior().read_memory(0x20000300, len(data))) == data
try:
    gdb.selected_inferior().write_memory(0x08000000, b'\\x00')
    raise AssertionError('Flash write unexpectedly accepted')
except gdb.MemoryError:
    pass
assert int(gdb.parse_and_eval('$pc')) == 0x08000012
assert 'maint flush register-cache' in gdb.execute('monitor reset', to_string=True)
gdb.execute('maint flush register-cache')
assert int(gdb.parse_and_eval('$pc')) == 0x08000000
gdb.execute('monitor reset nrst')
assert 'usage' in gdb.execute('monitor bogus', to_string=True)
print('SIDP_E2E_OK')
end
detach
quit
''')
        debugger = subprocess.Popen([gdb, "-q", "-nx", "-batch", "-x", str(script)],
                                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        processes.append(debugger)

        def interrupt_continue():
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline and debugger.poll() is None:
                log = (output / f"{label}-endpoint.log").read_text()
                if log.splitlines().count("REQUEST 32") >= 3:
                    time.sleep(0.1)
                    debugger.send_signal(signal.SIGINT)
                    return
                time.sleep(0.01)

        interrupter = threading.Thread(target=interrupt_continue, daemon=True)
        interrupter.start()
        stdout, stderr = debugger.communicate(timeout=30)
        interrupter.join(timeout=1)
        result = subprocess.CompletedProcess(debugger.args, debugger.returncode, stdout, stderr)
        (output / f"{label}-gdb.log").write_text(result.stdout + result.stderr)
        assert result.returncode == 0 and "SIDP_E2E_OK" in result.stdout, result.stdout + result.stderr
        assert "rejected" not in result.stderr.lower(), result.stderr
        assert proc.wait(timeout=5) == 0, "Agent failed after detach"
        endpoint_log.flush()
        requests = [int(line.split()[1]) for line in (output / f"{label}-endpoint.log").read_text().splitlines()
                    if line.startswith("REQUEST ")]
        assert requests[0] == 1 and requests[-1] == 2, requests
        # Initial registers must be served from STOPPED, not fetched over SIDP.
        first_write = requests.index(0x14)
        assert 0x13 not in requests[:first_write], requests
        assert requests.count(0x20) == 3 and 0x21 in requests, requests
        assert requests.count(0x22) == 2, requests
        print(f"PASS {label}: real GDB, fragmented CDC, C++ SIDP session ({len(requests)} requests)")
    finally:
        stop.set()
        for proc in reversed(processes):
            if proc.poll() is None:
                proc.terminate()
            try:
                proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=3)
        for thread in threads:
            thread.join(timeout=1)
        os.close(master)
        os.close(slave)
        endpoint_log.close()
        agent_log.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sidp-client", type=Path, required=True)
    parser.add_argument("--gdb", default=shutil.which("gdb-multiarch") or shutil.which("arm-none-eabi-gdb"))
    parser.add_argument("--output", type=Path, default=Path(tempfile.mkdtemp(prefix="sidp-e2e-")))
    args = parser.parse_args()
    if not args.gdb:
        parser.error("Provide --gdb pointing to an ARM-capable GDB")
    args.output.mkdir(parents=True, exist_ok=True)
    client = args.sidp_client.resolve()
    print(f"Logs: {args.output}", flush=True)
    subprocess.run(["cargo", "build", "--locked"], cwd=ROOT, check=True, timeout=120)
    endpoint = args.output / "mock-endpoint"
    subprocess.run(["c++", "-std=gnu++20", "-Wall", "-Wextra", "-Werror",
                    f"-I{client / 'include'}", f"-I{client / 'test/host_stubs'}",
                    str(Path(__file__).with_name("mock_endpoint.cpp")),
                    str(client / "sidp_session.cpp"), str(client / "sidp_transport.cpp"),
                    "-o", str(endpoint)], check=True, timeout=60)
    target_dir = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
    agent = target_dir / "debug/sidp-agent"
    for profile, fpu in [(1, False), (2, False), (3, False), (3, True)]:
        scenario(agent, endpoint, args.gdb, args.output, profile, fpu)
    scenarios(agent, endpoint, args.output, pump)


if __name__ == "__main__":
    main()
