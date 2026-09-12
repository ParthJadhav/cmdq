#!/usr/bin/env python3
"""Test the generated installer against local release artifacts before publishing."""
import functools
import hashlib
import http.server
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading

artifacts = Path(sys.argv[1]).resolve()
expected_binary = Path(os.environ["CMDQ_TEST_BINARY"])
expected_hash = hashlib.sha256(expected_binary.read_bytes()).digest()
handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=str(artifacts))
with http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler) as server:
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="cmdq-install-") as root:
            env = os.environ | {
                "CARGO_HOME": root,
                "CMDQ_NO_MODIFY_PATH": "1",
                "CMDQ_DOWNLOAD_URL": f"http://127.0.0.1:{server.server_port}",
            }
            # Fresh installation and repeat installation must both preserve the binary.
            for _ in range(2):
                subprocess.run(["sh", str(artifacts / "cmdq-installer.sh")], env=env, check=True)
                installed = Path(root) / "bin/cmdq"
                assert hashlib.sha256(installed.read_bytes()).digest() == expected_hash
                subprocess.run([str(installed), "--version"], check=True)
    finally:
        server.shutdown()
        thread.join()
