#!/usr/bin/env python3
"""Opt-in disposable SAB acceptance. Never builds/pulls; uses a prebuilt musl test.

Run with --allow-disposable-queue-writes --test-binary /path/to/disposable_sabnzbd.
Requires the pinned image locally. Reuses the qBittorrent harness's bounded Docker
commands, ELF validation and label-checked cleanup, without running its scenario.
Pipeline mode additionally requires --pipeline --helper-image with an existing
application runtime and the separate disposable_sabnzbd_pipeline musl executable.
Both modes retain private evidence. Pipeline cases have 180-second deadlines,
560 seconds overall; no production features or browser coverage are claimed.
No host-root, published ports, production config, Docker socket mounts or nsenter.
Upstream paths verified from:
https://github.com/linuxserver/docker-sabnzbd/blob/5.1.3-ls272/Dockerfile
https://github.com/linuxserver/docker-sabnzbd/blob/5.1.3-ls272/root/etc/s6-overlay/s6-rc.d/svc-sabnzbd/run
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import secrets
import shutil
import signal
import subprocess
import sys

sys.dont_write_bytecode = True


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


HERE = Path(__file__).resolve().parent
common = load("disposable_common", HERE / "check-qbittorrent.py")
fixture = load("owned_nntp", HERE / "fixtures/nntp.py")
IMAGE = ("linuxserver/sabnzbd:5.1.3-ls272@"
         "sha256:7f173ced3541b46c57c4eb6854765243095c8c8a7563a878044fa0ccfb1f4004")
# Isolated imported module; its cleanup must recognize only SAB labels for this run.
common.IMAGE = IMAGE
common.LABEL = "libraryd.disposable-sabnzbd"


class Harness(common.Harness):
    def __init__(self, binary, helper_image, pipeline=False):
        super().__init__(binary, helper_image)
        self.pipeline = pipeline
        self.runtime_image_id = None
        self.cpu = str(min(os.sched_getaffinity(0)))

    def verify_isolation(self, item):
        state = super().verify_isolation(item)
        host = state["HostConfig"]
        if (host.get("Memory") != 512 * 1024 * 1024
                or host.get("MemorySwap") != 512 * 1024 * 1024
                or host.get("CapDrop") != ["ALL"]
                or host.get("CpusetCpus") != self.cpu):
            raise RuntimeError("owned container resource/capability mismatch")
        if "mounts" in item:
            actual = {(m["Source"], m["Destination"], not m["RW"]) for m in state["Mounts"]}
            if actual != {tuple(m) for m in item["mounts"]}:
                raise RuntimeError("owned container bind mount mismatch")
        return state

    def create_args(self, item):
        args = super().create_args(item)
        return ["512m" if value == "256m" else value for value in args] + ["--cpuset-cpus", self.cpu]

    def create(self, role, network, cpu, command, mounts, image_id=None):
        item = {"name": f"library-sab-{self.run_id}-{role}", "role": role,
                "network": network, "nano_cpus": cpu, "image_id": image_id or self.helper_image_id}
        item["mounts"] = [(str(self.root / role), "/config", False)] + [
            (str(source), target, readonly) for source, target, readonly in mounts]
        self.owned.append(item)
        self.record()
        args = self.create_args(item) + ["--workdir", "/config", "--env", "HOME=/config",
            "--env", "PYTHONDONTWRITEBYTECODE=1", "--env", "PATH=/lsiopy/bin:/usr/local/bin:/usr/bin:/bin",
            "--mount", f"type=bind,src={self.root / role},dst=/config"]
        for source, target, readonly in mounts:
            args += ["--mount", f"type=bind,src={source},dst={target}" + (",readonly" if readonly else "")]
        if role in ("sab", "nntp") or (role == "helper" and not self.pipeline):
            command = ["/lsiopy/bin/python3", "-c",
                "import os,sys; os.setpriority(os.PRIO_PROCESS,0,8); os.execv(sys.argv[1],sys.argv[1:])"] + command
        args += ["--entrypoint", command[0], item["image_id"]] + command[1:]
        self.command(args)
        self.verify_isolation(item)
        return item

    def run(self):
        self.helper_image_id = self.local_image_id(IMAGE)
        if self.pipeline:
            self.runtime_image_id = self.local_image_id(self.helper_image)
        self.record()
        for role in ("sab", "nntp", "helper", "downloads", "control"):
            (self.root / role).mkdir(mode=0o700)
        # Snapshot the fixture so active runs cannot observe concurrent repository edits.
        nntp_script = self.root / "nntp-fixture.py"
        nntp_script.write_bytes((HERE / "fixtures/nntp.py").read_bytes())
        key, nzb_key = secrets.token_hex(16), secrets.token_hex(16)
        self.secrets.extend([key, nzb_key])
        category = "owned-" + self.run_id
        config = f"""[misc]
config_version = 19
host = 127.0.0.1
port = 8080
api_key = {key}
nzb_key = {nzb_key}
inet_exposure = 0
download_dir = /downloads/incomplete
complete_dir = /downloads/complete
cache_dir = /config/cache
log_dir = /config/logs
admin_dir = /config/admin
permissions = 0700
check_new_rel = 0
start_paused = 0
pre_check = 0
safe_postproc = 0
replace_spaces = 0
deobfuscate_final_filenames = 0
auto_disconnect = 0
[servers]
[[owned]]
name = owned
host = 127.0.0.1
port = 8119
connections = 1
ssl = 0
enable = 1
optional = 0
retention = 0
[categories]
[[{category}]]
name = {category}
pp = 0
script = None
dir = owned
priority = 0
"""
        (self.root / "sab/sabnzbd.ini").write_text(config)
        fixture.generate(self.root / "control")
        if self.pipeline:
            fixture_root = HERE.parent / "tests/fixtures"
            for case, source in [("comic", "natural-order.cbz"), ("manga", "natural-order.cbz"), ("magazine", "single-page.pdf")]:
                directory = self.root / "control" / case
                directory.mkdir(mode=0o700)
                name = case + Path(source).suffix
                payload = (fixture_root / source).read_bytes()
                if not 2 <= len(payload) <= 1024 * 1024:
                    raise ValueError("owned fixture size is out of bounds")
                (directory / name).write_bytes(payload)
                fixture.generate(directory, payload, name, case)
            common.private_json(self.root / "control/pipeline-boundaries.json", {
                "cases": ["comic:cbz", "manga:cbz", "magazine:pdf"],
                "deadline_seconds_per_case": 180, "total_deadline_seconds": 560,
                "network": "owned SAB namespace only", "cpu_total": 0.5,
                "source": "/downloads read-only in helper", "destination": "/config/{case}/library",
                "association": "explicit after Downloaded", "import_policy": "copy",
                "evidence": "real receipts, journal Done, exact bytes, reader pages; no browser claim"})
        sab = self.create("sab", "none", 350_000_000,
            ["/lsiopy/bin/python3", "/app/sabnzbd/SABnzbd.py", "--config-file", "/config/sabnzbd.ini",
             "--server", "127.0.0.1:8080", "--browser", "0"],
            [(self.root / "downloads", "/downloads", False)])
        self.command(self.docker + ["start", sab["id"]])
        self.verify_isolation(sab)
        network = f"container:{sab['id']}"
        nntp = self.create("nntp", network, 50_000_000,
            ["/lsiopy/bin/python3", "/nntp.py", "--root", "/control"] + (["--pipeline"] if self.pipeline else []),
            [(nntp_script, "/nntp.py", True), (self.root / "control", "/control", False)])
        self.command(self.docker + ["start", nntp["id"]])
        _, netns = self.command(self.docker + ["exec", sab["id"], "/bin/busybox", "readlink", "/proc/self/ns/net"])
        common.private_json(self.root / "control/acceptance.json", {
            "run_id": self.run_id, "key": key, "nzb_key": nzb_key,
            "sha256": hashlib.sha256(fixture.PAYLOAD).hexdigest(), "netns": netns.strip()})
        helper = self.create("helper", network, 100_000_000,
            ["/bin/busybox", "env", "LIBRARY_DISPOSABLE_SAB=I_ACCEPT_OWNED_QUEUE_WRITES",
             "/acceptance-test", "--ignored", "--exact",
             "disposable_owned_sabnzbd_pipeline" if self.pipeline else "disposable_owned_sabnzbd",
             "--test-threads=1", "--nocapture"],
            [(self.binary, "/acceptance-test", True), (self.root / "control", "/control", False),
             (self.root / "downloads", "/downloads", True)], image_id=self.runtime_image_id)
        self.verify_isolation(nntp)
        self.command(self.docker + ["start", "--attach", helper["id"]], timeout=570 if self.pipeline else 190)
        state = self.verify_isolation(helper)
        if state["State"]["Running"] or state["State"]["ExitCode"] != 0:
            raise RuntimeError("acceptance helper failed")
        result = json.loads((self.root / "control/result.json").read_text())
        if result.get("run_id") != self.run_id or result.get("passed") is not True:
            raise RuntimeError("missing matching real-transfer evidence")
        if self.pipeline:
            cases = result.get("cases")
            if not isinstance(cases, list) or len(cases) != 3:
                raise RuntimeError("missing three-content pipeline evidence")
            acquisitions = set()
            for case, expected_name in zip(cases, ("comic", "manga", "magazine"), strict=True):
                if (case.get("case") != expected_name or case.get("passed") is not True
                        or case.get("journal") != "done" or case.get("receipts") != 1
                        or case.get("source_preserved") is not True
                        or case.get("explicit_association") is not True
                        or case.get("reopen_verified") is not True
                        or case.get("source_sha256") != case.get("sha256")
                        or case.get("reader_pages") != (1 if expected_name == "magazine" else 3)):
                    raise RuntimeError("incomplete pipeline transfer/import/reader evidence")
                acquisitions.add(case["acquisition_id"])
            if len(acquisitions) != 3:
                raise RuntimeError("pipeline cases did not create distinct acquisitions")
        print("Owned SAB acceptance passed; artifacts retained.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--allow-disposable-queue-writes", action="store_true")
    parser.add_argument("--test-binary", type=Path, required=True)
    parser.add_argument("--pipeline", action="store_true", help="run the separate three-content pipeline test")
    parser.add_argument("--helper-image", help="existing local app runtime with reader tools; required for --pipeline")
    args = parser.parse_args()
    if args.pipeline != bool(args.helper_image):
        parser.error("--pipeline and --helper-image must be supplied together")
    if not args.allow_disposable_queue_writes:
        parser.error("explicit queue-write opt-in required")
    binary = args.test_binary.resolve(strict=True)
    if not binary.is_file() or not os.access(binary, os.X_OK) or not shutil.which("docker"):
        parser.error("requires Docker and a prebuilt executable; no build/pull fallback")
    common.check_test_binary(binary)
    os.umask(0o077)
    os.setpriority(os.PRIO_PROCESS, 0, 8)
    harness = Harness(binary, args.helper_image or IMAGE, args.pipeline)
    def interrupted(_signal, _frame):
        raise InterruptedError("acceptance interrupted")
    signal.signal(signal.SIGINT, interrupted)
    signal.signal(signal.SIGTERM, interrupted)
    passed = False
    try:
        harness.run()
        passed = True
    except (RuntimeError, OSError, ValueError, KeyError, subprocess.TimeoutExpired) as error:
        print(f"Acceptance failed: {type(error).__name__}; inspect private command logs")
        if isinstance(error, RuntimeError):
            # Harness RuntimeErrors contain operation/log names, never API bodies or credentials.
            print(str(error))
    finally:
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        clean = harness.cleanup()
        common.private_json(harness.root / "harness-result.json", {"passed": passed, "cleanup_complete": clean})
    return 0 if passed and clean else 1


if __name__ == "__main__":
    raise SystemExit(main())
