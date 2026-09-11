import http.server
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time


def command(*args):
    return subprocess.check_output(args, text=True).strip()


def main():
    compute = os.environ.get("NIX_COMPUTE_BIN", "target/debug/nix-compute")
    provider = os.environ.get(
        "NIX_COMPUTE_PROVIDER_BIN", "target/debug/nix-compute-provider-reference"
    )
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        flake = root / "flake"
        flake.mkdir()
        source = json.loads(command("nix", "flake", "archive", "--json", "."))["path"]
        template = Path(__file__).parent / "lifecycle-fixture" / "flake.nix"
        (flake / "flake.nix").write_text(
            template.read_text().replace("@COMPUTE_SOURCE@", source)
        )
        subprocess.run(["nix", "flake", "lock", f"path:{flake}"], check=True)
        key = root / "key.json"
        command(provider, "keygen", str(key))

        def build(scenario):
            return command(compute, "build", f"path:{flake}#{scenario}", "--target", "cpu")

        def launch_args(task, scenario, rank):
            context = root / f"{scenario}-{rank}.json"
            context.write_text(json.dumps({
                "run_id": scenario, "node_id": f"node-{rank}", "node_rank": rank,
                "node_count": 2, "master_addr": "127.0.0.1", "master_port": 29500,
            }))
            return [
                provider, "run", task, "--context", str(context),
                "--signing-key", str(key), "--coordination-dir", str(root / "group"),
                "--coordination-timeout", "2", "--state-dir", str(root / "runs"),
            ]

        def report(scenario, rank):
            path = root / f"runs/{scenario}/node-{rank}/attestation.json"
            return json.loads(path.read_text())["payload"]

        preparation = subprocess.run(
            launch_args(build("preparation"), "preparation", 0),
            capture_output=True, text=True, timeout=15,
        )
        assert preparation.returncode != 0, preparation.stdout
        payload = report("preparation", 0)
        assert payload["status"] == "failed", payload
        assert payload["exit_code"] is None, payload
        assert "insufficient CPU cores" in payload["error"], payload

        received = threading.Event()
        release = threading.Event()

        class Gateway(http.server.BaseHTTPRequestHandler):
            def do_PUT(self):
                received.set()
                release.wait(15)
                self.send_response(200)
                self.send_header("Content-Length", "0")
                self.end_headers()

            def log_message(self, *_args):
                pass

        task = build("upload")
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Gateway)
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        processes = []
        try:
            env = dict(os.environ, NIX_COMPUTE_S3_ENDPOINT=f"http://127.0.0.1:{server.server_port}")
            for rank in [0, 1]:
                processes.append(subprocess.Popen(
                    launch_args(task, "upload", rank), env=env,
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                ))
            assert received.wait(15), "failed node never reached output upload"
            peer_out, peer_err = processes[1].communicate(timeout=5)
            assert processes[1].returncode != 0, (peer_out, peer_err)
            assert processes[0].poll() is None, "upload was not blocked"
            assert "peer-started" in (root / "runs/upload/node-1/stdout.log").read_text()
            assert report("upload", 1)["status"] == "cancelled"
            release.set()
            processes[0].communicate(timeout=10)
            payload = report("upload", 0)
            assert processes[0].returncode != 0
            assert payload["status"] == "failed", payload
            assert payload["exit_code"] == 1, payload
            assert len(payload["outputs"]) == 1, payload
        finally:
            release.set()
            for process in processes:
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.communicate(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.communicate()
            server.shutdown()
            server.server_close()
            worker.join()

        task = build("reporting")
        for blocked_rank in [0, 1]:
            scenario = f"reporting-{blocked_rank}"
            blocker = root / f"runs/{scenario}/node-{blocked_rank}/attestation.json"
            blocker.mkdir(parents=True)
            processes = []
            try:
                for rank in [0, 1]:
                    processes.append(subprocess.Popen(
                        launch_args(task, scenario, rank),
                        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                    ))
                for process in processes:
                    out, err = process.communicate(timeout=15)
                    assert process.returncode != 0, (out, err)
                assert report(scenario, 1 - blocked_rank)["status"] != "succeeded"
                member = json.loads((root / f"group/{scenario}/{blocked_rank}.json").read_text())
                assert member["phase"] == "failed", member
            finally:
                for process in processes:
                    if process.poll() is None:
                        process.kill()
                        process.communicate()
        task = build("provisional")
        processes = []
        try:
            for rank in [0, 1]:
                processes.append(subprocess.Popen(
                    launch_args(task, "provisional", rank),
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                ))
            early = root / "runs/provisional/node-0/attestation.json"
            deadline = time.monotonic() + 15
            while not early.exists() and time.monotonic() < deadline:
                time.sleep(0.05)
            assert report("provisional", 0)["status"] == "prepared"
            assert processes[1].poll() is None
            processes[0].kill()
            processes[0].communicate(timeout=5)
            processes[1].communicate(timeout=8)
            assert processes[1].returncode != 0
            assert report("provisional", 1)["status"] != "succeeded"
            assert report("provisional", 0)["status"] == "prepared"
        finally:
            for process in processes:
                if process.poll() is None:
                    process.kill()
                    process.communicate()

        if os.geteuid() != 0:
            for scenario in ["local-readonly", "shared-final-readonly", "shared-parent-unreadable"]:
                processes = []
                blocked = None
                try:
                    for rank in [0, 1]:
                        processes.append(subprocess.Popen(
                            launch_args(task, scenario, rank),
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                        ))
                    early = root / f"runs/{scenario}/node-0/attestation.json"
                    deadline = time.monotonic() + 15
                    while not early.exists() and time.monotonic() < deadline:
                        time.sleep(0.05)
                    assert report(scenario, 0)["status"] == "prepared"
                    if scenario == "shared-parent-unreadable":
                        blocked = root / "group"
                        blocked.chmod(0o300)
                    else:
                        blocked = (early.parent if scenario == "local-readonly"
                                   else root / f"group/{scenario}/reports/final")
                        blocked.chmod(0o500)
                    for process in processes:
                        out, err = process.communicate(timeout=15)
                        assert (process.returncode == 0) == (scenario == "local-readonly"), (out, err)
                    for rank in [0, 1]:
                        assert (report(scenario, rank)["status"] == "succeeded") == (scenario == "local-readonly")
                finally:
                    if blocked is not None:
                        blocked.chmod(0o700)
                    for process in processes:
                        if process.poll() is None:
                            process.kill()
                            process.communicate()

        task = build("automatic")
        processes = []
        try:
            for index in [0, 1]:
                processes.append(subprocess.Popen([
                    provider, "run", task, "--signing-key", str(key),
                    "--state-dir", str(root / f"automatic-{index}"),
                ], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True))
            for process in processes:
                out, err = process.communicate(timeout=15)
                assert process.returncode == 0, (out, err)
            ports = [path.read_text() for path in root.glob("automatic-*/*/node-0/outputs/port")]
            assert len(ports) == 2 and ports[0] != ports[1], ports
        finally:
            for process in processes:
                if process.poll() is None:
                    process.kill()
                    process.communicate()
    print("Provider lifecycle, provisional reports and concurrent rendezvous checks passed")


if __name__ == "__main__":
    main()
