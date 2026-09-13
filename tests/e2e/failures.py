"""Fatal connection scenarios against the CLI and real C++ SIDP session."""
import os
import pty
import select
import socket
import subprocess
import threading
import tty


def scenarios(agent, endpoint, output, pump):
    """USB loss, corrupt incoming frame, and debugger EOF must terminate promptly."""
    for fault in ("usb-loss", "bad-crc", "gdb-eof"):
        master, slave = pty.openpty()
        tty.setraw(slave)
        stop = threading.Event()
        processes, threads = [], []
        try:
            with open(output / f"{fault}-endpoint.log", "wb") as mock_log, \
                    open(output / f"{fault}-agent.log", "wb") as agent_log:
                mock = subprocess.Popen([str(endpoint), "3", "basic"], stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=mock_log)
                processes.append(mock)
                for src, dst in ((master, mock.stdin.fileno()), (mock.stdout.fileno(), master)):
                    thread = threading.Thread(target=pump, args=(src, dst, stop), daemon=True)
                    thread.start()
                    threads.append(thread)
                proc = subprocess.Popen([str(agent), "gdb", "--port", os.ttyname(slave),
                                         "--listen", "127.0.0.1:0", "--no-dtr", "--settle-ms", "0"],
                                        stdout=subprocess.PIPE, stderr=agent_log)
                processes.append(proc)
                assert select.select([proc.stdout], [], [], 10)[0], "Agent readiness timeout"
                line = proc.stdout.readline().decode().strip()
                assert line.startswith("GDB listening on "), line
                address = line.removeprefix("GDB listening on ")
                if fault == "usb-loss":
                    stop.set()
                    for thread in threads:
                        thread.join(timeout=1)
                    os.close(master)
                    master = None
                elif fault == "bad-crc":
                    # A valid-sized frame with a deliberately invalid CRC.
                    os.write(master, b'\xc0\x01\x03\x80\x00' + bytes(8) + b'\xc0')
                else:
                    host, port = address.rsplit(":", 1)
                    with socket.create_connection((host, int(port)), timeout=3):
                        pass
                assert proc.wait(timeout=5) != 0, f"{fault}: CLI incorrectly reported success"
                mock_log.flush()
                requests = [line for line in (output / f"{fault}-endpoint.log").read_text().splitlines()
                            if line.startswith("REQUEST ")]
                assert requests == ["REQUEST 1"], f"{fault}: requests replayed: {requests}"
                print(f"PASS {fault}: process exits, no operation replay")
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
            if master is not None:
                os.close(master)
            os.close(slave)
