import hashlib
import http.server
import json
import os
from pathlib import Path
import signal
import subprocess
import sys

with open(sys.argv[1]) as source:
    config = json.load(source)
worker = None
receipt = None
starts = 0
posts = []
fail_lookup_once = False


def group_live():
    if worker is None:
        return False
    for path in Path("/proc").glob("[0-9]*/stat"):
        try:
            fields = path.read_text().rsplit(")", 1)[1].split()
            if int(fields[2]) == worker.pid and fields[0] != "Z":
                return True
        except (FileNotFoundError, ProcessLookupError):
            pass
    return False


def stop():
    if worker is not None and worker.poll() is None:
        os.killpg(worker.pid, signal.SIGTERM)
        try:
            worker.wait(timeout=8)
        except subprocess.TimeoutExpired:
            os.killpg(worker.pid, signal.SIGKILL)
            worker.wait(timeout=3)


def operation(kind, fingerprint=None):
    return dict(id="fixture-operation", operationKey="fixture-operation", idempotencyKey=config["launch_id"],
                requestFingerprint=fingerprint, actor="owned-fixture", kind=kind,
                environmentKey="fixture", workspaceKey=None, sessionKey="fixture-session",
                state="succeeded", queuedAt="2026-01-01T00:00:00Z", startedAt=None,
                completedAt=None, result={"sessionId": "fixture-session"},
                errorCode=None, errorMessage=None, retryable=False)


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, value, status=200):
        body = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        global fail_lookup_once
        if self.path == "/proof":
            return self.reply(dict(starts=starts, posts=posts, receipt=receipt,
                                   worker_live=worker is not None and worker.poll() is None,
                                   worker_group_live=group_live()))
        if self.headers.get("Authorization") != "Bearer owned-fixture-token":
            return self.reply({}, 401)
        if self.path == "/api/pulse/environments/fixture/managed-launch-options/hook-worker":
            return self.reply(dict(environmentId="fixture", presetId="hook-worker",
                                   daemonAddress=config["daemon"], hookBase=config["hook"]))
        if self.path == "/api/pulse/environments/fixture/managed-launches/" + config["launch_id"]:
            if fail_lookup_once:
                fail_lookup_once = False
                return self.reply({"error": "owned transport fault after physical start"}, 503)
            return self.reply(receipt, 200 if receipt else 404)
        if self.path == "/api/pulse/sessions/fixture-session/runtime":
            return self.reply(dict(sessionId="fixture-session", availability=(
                "live" if worker is not None and worker.poll() is None else "retained"),
                claimPhase=None, recordedImage=None, desiredImage=None, actions=[]))
        self.reply({}, 404)

    def do_POST(self):
        global worker, receipt, starts, fail_lookup_once
        if self.headers.get("Authorization") != "Bearer owned-fixture-token":
            return self.reply({}, 401)
        if self.path == "/api/pulse/sessions/fixture-session/archive":
            assert self.headers["Idempotency-Key"].startswith("marshal-stop-")
            stop()
            return self.reply(dict(operation=operation("archiveSession")))
        if self.path != "/api/pulse/environments/fixture/managed-launches":
            return self.reply({}, 404)
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        expected = dict(launchId=config["launch_id"], presetId="hook-worker",
                        workspace={"provider": "session"})
        assert body == expected, body
        posts.append(body)
        if receipt is None:
            env = dict(PATH="/usr/bin:/bin", HOME=config["home"],
                       XDG_CONFIG_HOME=config["home"] + "/config",
                       XDG_STATE_HOME=config["home"] + "/state",
                       MARSHAL_E2E_SHIM=config["shim"], MARSHAL_BASE_URL=config["hook"],
                       MARSHAL_DAEMON_ADDRESS=config["daemon"],
                       MARSHAL_AGENT_ID=config["launch_id"])
            worker = subprocess.Popen(["/bin/sh", config["worker"]], env=env,
                                      cwd=config["cwd"], stdout=log, stderr=log,
                                      start_new_session=True)
            starts += 1
            wire = json.dumps(["fixture", expected], separators=(",", ":")).encode()
            receipt = dict(launchId=config["launch_id"], state="ready", turnSubmitted=False,
                           identity=dict(sessionId="fixture-session", host=config["host"],
                                         cwd=config["cwd"]),
                           operation=operation("launchAgent", hashlib.sha256(wire).hexdigest()))
            fail_lookup_once = True
        self.reply(receipt)


def terminate(*_):
    raise SystemExit(0)


signal.signal(signal.SIGTERM, terminate)
signal.signal(signal.SIGINT, terminate)
with open(config["log"], "wb", buffering=0) as log:
    server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
    with open(config["ready"], "w") as ready:
        ready.write("http://127.0.0.1:" + str(server.server_port))
    try:
        server.serve_forever()
    finally:
        stop()
        server.server_close()
