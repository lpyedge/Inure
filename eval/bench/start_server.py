#!/usr/bin/env python3
"""start_server.py [port]: start eval/bench/server.py in the background, PID in eval/bench/server.pid, wait until it answers."""
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
PIDFILE = os.path.join(HERE, "server.pid")
LOG = os.path.join(HERE, "logs", "server.log")


def main():
    port = sys.argv[1] if len(sys.argv) > 1 else "18295"
    if os.path.exists(PIDFILE):
        raise SystemExit(f"{PIDFILE} exists: run stop_server.sh first")
    os.makedirs(os.path.dirname(LOG), exist_ok=True)
    with open(LOG, "ab") as log:
        p = subprocess.Popen([sys.executable, os.path.join(HERE, "server.py"), port],
                             stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    with open(PIDFILE, "w", encoding="utf-8") as f:
        f.write(str(p.pid) + "\n")
    deadline = time.time() + 10
    while time.time() < deadline:
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{port}/index.html", timeout=1).close()
            print(f"server pid={p.pid} port={port} ok")
            return 0
        except (urllib.error.URLError, OSError):
            time.sleep(0.2)
    print(f"server not answering on {port}; see {LOG}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
