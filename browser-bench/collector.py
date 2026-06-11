#!/usr/bin/env python3
"""Benchmark result collector: the c2w browser page POSTs one JSON result
per run to http://localhost:8081/result?tag=...; appended as JSON lines to
/tmp/c2w-results/results.jsonl with arrival timestamp."""
import http.server
import json
import os
import time

OUTDIR = "/tmp/c2w-results"
OUTFILE = os.path.join(OUTDIR, "results.jsonl")
os.makedirs(OUTDIR, exist_ok=True)


class H(http.server.BaseHTTPRequestHandler):
    def _cors(self):
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Methods", "POST, GET, OPTIONS")
        self.send_header("Access-Control-Allow-Headers", "Content-Type")

    def do_OPTIONS(self):
        self.send_response(204)
        self._cors()
        self.end_headers()

    def do_GET(self):
        body = b"[]"
        if os.path.exists(OUTFILE):
            with open(OUTFILE, "rb") as f:
                body = b"[" + b",".join(l for l in f.read().splitlines() if l) + b"]"
        self.send_response(200)
        self._cors()
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(n).decode("utf-8", "replace")
        try:
            obj = json.loads(raw)
        except Exception:
            obj = {"raw": raw}
        obj["_tag"] = self.path
        obj["_ts"] = time.strftime("%H:%M:%S")
        with open(OUTFILE, "a") as f:
            f.write(json.dumps(obj) + "\n")
        print(f"[collector] {obj.get('_ts')} {self.path} "
              f"runMs={obj.get('runMs')} exit={obj.get('exitCode')}", flush=True)
        self.send_response(200)
        self._cors()
        self.end_headers()
        self.wfile.write(b"ok")

    def log_message(self, *a):
        pass


http.server.ThreadingHTTPServer(("0.0.0.0", 8081), H).serve_forever()
