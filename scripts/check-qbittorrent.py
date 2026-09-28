#!/usr/bin/env python3
"""Explicitly opt-in disposable qBittorrent acceptance; never builds or pulls.

Prerequisites: access to rootful local Docker at /var/run/docker.sock with user
namespace remapping disabled, the pinned qBittorrent image already present, a local
Alpine runtime image, and a prebuilt musl disposable_qbittorrent test binary.
Rootless/remapped daemons are rejected before container creation. Remote Docker
contexts are not used; bind mounts and container UID/GID require local identity mapping.
No host-root access is required. Clients receive 0.2 CPU each; the helper gets 0.1.
Example (replace the prebuilt binary path):
  rtk proxy nice -n 8 python3 \
    scripts/check-qbittorrent.py --allow-disposable-queue-writes \
    --test-binary /absolute/path/disposable_qbittorrent-HASH \
    --helper-image libraryd:local-check

Build the test separately in the repository's Alpine backend stage, using
--release --features embedded-ui --test disposable_qbittorrent --no-run --locked
--offline -j 2. Export its executable, not its .d file or the host GNU executable.
The helper image must provide Alpine/musl, libgcc, and /bin/busybox; the existing
final application image does. Its entrypoint and healthcheck are disabled.

Keeps its private run directory, sanitized logs, hashes, and recorded container IDs.
Cleanup addresses only recorded IDs with this run's ownership label. No pruning.
Pinned acceptance image; upstream configuration references:
https://github.com/qbittorrent/docker-qbittorrent-nox/blob/main/entrypoint.sh
https://github.com/qbittorrent/qBittorrent/blob/release-5.2.3/src/base/utils/password.cpp
"""

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import resource
import secrets
import shutil
import signal
import struct
import subprocess
import tempfile
import uuid


IMAGE = ("qbittorrentofficial/qbittorrent-nox:5.2.3-1@"
         "sha256:9ebb534fe30bab98622cb84a8c3acecfd88319b2d540f52ecdec7b9f866374d7")
LABEL = "libraryd.disposable-qbittorrent"
OPT_IN = "I_ACCEPT_OWNED_QUEUE_WRITES"
LOG_LIMIT = 2 * 1024 * 1024


def check_test_binary(binary: Path) -> None:
    """Reject a host GNU dynamic loader without executing the supplied artifact."""
    if any(character in str(binary) for character in ",\r\n"):
        raise ValueError("test binary path must be safe for an exact Docker bind mount")
    with binary.open("rb") as stream:
        header = stream.read(64)
        if len(header) != 64 or header[:6] != b"\x7fELF\x02\x01":
            raise ValueError("expected a native 64-bit little-endian Alpine/musl ELF executable")
        machine = struct.unpack_from("<H", header, 18)[0]
        expected = {"x86_64": 62, "aarch64": 183}.get(os.uname().machine)
        if machine != expected:
            raise ValueError("test executable architecture does not match the local runtime")
        offset = struct.unpack_from("<Q", header, 32)[0]
        size, count = struct.unpack_from("<HH", header, 54)
        if offset > 1024 * 1024 or not 56 <= size <= 128 or not 1 <= count <= 256:
            raise ValueError("invalid ELF program headers")
        for index in range(count):
            stream.seek(offset + index * size)
            program = stream.read(56)
            if len(program) != 56:
                raise ValueError("truncated ELF executable")
            if struct.unpack_from("<I", program)[0] != 3:
                continue
            position = struct.unpack_from("<Q", program, 8)[0]
            length = struct.unpack_from("<Q", program, 32)[0]
            if not 1 <= length <= 256:
                raise ValueError("invalid ELF interpreter")
            stream.seek(position)
            interpreter = stream.read(length).rstrip(b"\0")
            allowed = {62: b"/lib/ld-musl-x86_64.so.1", 183: b"/lib/ld-musl-aarch64.so.1"}
            if interpreter != allowed[machine]:
                raise ValueError("host GNU executable supplied; export the Alpine backend's musl test binary")


def encode(value: bytes | int | dict) -> bytes:
    if isinstance(value, bytes):
        return str(len(value)).encode() + b":" + value
    if isinstance(value, int):
        return b"i" + str(value).encode() + b"e"
    return b"d" + b"".join(encode(key) + encode(value[key]) for key in sorted(value)) + b"e"


def private_json(path: Path, value: object) -> None:
    pending = path.with_suffix(path.suffix + ".pending")
    with pending.open("w", encoding="utf-8") as stream:
        json.dump(value, stream, indent=2)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    pending.replace(path)
    descriptor = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def qbit_config(port: int, peer: int, password: str) -> str:
    salt = secrets.token_bytes(16)
    key = hashlib.pbkdf2_hmac("sha512", password.encode(), salt, 100000, 64)
    secret = base64.b64encode(salt).decode() + ":" + base64.b64encode(key).decode()
    # Fresh profiles only. API setup below additionally verifies effective settings.
    return f"""[LegalNotice]
Accepted=true
[BitTorrent]
Session\\DefaultSavePath=/downloads/
Session\\TempPathEnabled=false
Session\\Port={peer}
Session\\DHTEnabled=false
Session\\PeXEnabled=false
Session\\LSDEnabled=false
Session\\UPnPEnabled=false
[Preferences]
WebUI\\Address=127.0.0.1
WebUI\\Port={port}
WebUI\\Username=acceptance
WebUI\\Password_PBKDF2=@ByteArray({secret})
WebUI\\LocalHostAuth=true
WebUI\\SecureCookie=false
WebUI\\AuthSubnetWhitelistEnabled=false
WebUI\\UseUPnP=false
WebUI\\CSRFProtection=true
WebUI\\HostHeaderValidation=true
Connection\\UPnP=false
"""


class Harness:
    def __init__(self, binary: Path, helper_image: str):
        self.binary = binary
        self.helper_image = helper_image
        self.helper_image_id = None
        self.qbit_image_id = None
        self._daemon_checked = False
        self.user = f"{os.getuid()}:{os.getgid()}"
        self.run_id = str(uuid.uuid4())
        self.root = Path(tempfile.mkdtemp(prefix="library-qbit-"))
        if any(character in str(self.root) for character in ",\r\n"):
            raise ValueError("temporary directory path is unsafe for Docker bind mounts")
        self.owned: list[dict] = []
        self.counter = 0
        self.secrets: list[str] = []
        self.env = {"PATH": "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                    "HOME": str(self.root), "LANG": "C.UTF-8", "TMPDIR": str(self.root)}
        self.docker = [shutil.which("docker"), "--host", "unix:///var/run/docker.sock",
                       "--config", str(self.root / "docker-cli")]
        (self.root / "docker-cli").mkdir(mode=0o700)
        print(f"Disposable run artifacts: {self.root}", flush=True)

    def record(self) -> None:
        private_json(self.root / "owned-containers.json", {
            "run_id": self.run_id, "image": IMAGE, "helper_image_id": self.helper_image_id,
            "containers": self.owned,
        })

    def command(self, args: list[str], timeout: int = 20, env: dict | None = None,
                check: bool = True) -> tuple[int, str]:
        self.counter += 1
        log = self.root / f"command-{self.counter:03}.log"

        def limits():
            os.setpriority(os.PRIO_PROCESS, 0, 8)
            resource.setrlimit(resource.RLIMIT_FSIZE, (LOG_LIMIT, LOG_LIMIT))
            resource.setrlimit(resource.RLIMIT_CORE, (0, 0))

        primary = None
        output = ""
        try:
            with log.open("wb") as stream:
                process = subprocess.Popen(args, stdin=subprocess.DEVNULL, stdout=stream,
                                           stderr=subprocess.STDOUT, env=env or self.env,
                                           start_new_session=True, preexec_fn=limits)
                try:
                    code = process.wait(timeout=timeout)
                except BaseException as error:
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    except BaseException as cleanup_error:
                        error.add_note(f"child stop failed: {type(cleanup_error).__name__}")
                    try:
                        process.wait(timeout=10)
                    except BaseException as cleanup_error:
                        error.add_note(f"child reap failed: {type(cleanup_error).__name__}")
                    raise
        except BaseException as error:
            primary = error
            raise
        finally:
            # Runs after normal exit or bounded kill/reap, including signal/timeout paths.
            # Cleanup diagnostics must not replace the original command exception.
            try:
                output = log.read_bytes()[:LOG_LIMIT].decode("utf-8", errors="replace")
                sanitized = output
                for secret in self.secrets:
                    sanitized = sanitized.replace(secret, "[redacted]")
                sanitized = "\n".join("[credential line redacted]" if any(
                    key in line.lower() for key in ("password", "set-cookie", "authorization:"))
                    else line for line in sanitized.splitlines())
                log.write_text(sanitized + "\n")
            except BaseException as cleanup_error:
                if primary is None:
                    raise
                primary.add_note(f"log sanitization failed: {type(cleanup_error).__name__}")
        if check and code != 0:
            raise RuntimeError(f"command failed ({code}); inspect {log.name}")
        return code, output

    def inspect_owned(self, item: dict) -> dict | None:
        code, raw = self.command(self.docker + ["inspect", "--type", "container", item.get("id", item["name"])], check=False)
        if code != 0:
            # Absence is distinguished from daemon/permission failure by the caller's log.
            if "No such" in raw:
                return None
            raise RuntimeError("could not inspect recorded disposable container")
        data = json.loads(raw)[0]
        if data["Config"].get("Labels", {}).get(LABEL) != self.run_id:
            raise RuntimeError("refusing cleanup: container ownership label mismatch")
        if not re.fullmatch(r"[0-9a-f]{64}", data["Id"]):
            raise RuntimeError("invalid disposable container ID")
        item["id"] = data["Id"]
        self.record()
        return data

    def local_image_id(self, reference: str) -> str:
        self.check_local_daemon()
        _, value = self.command(self.docker + ["image", "inspect", "--format", "{{.Id}}", reference])
        image_id = value.strip()
        if not re.fullmatch(r"sha256:[0-9a-f]{64}", image_id):
            raise RuntimeError("expected an existing local image ID; no pull fallback")
        return image_id

    def check_local_daemon(self) -> None:
        if self._daemon_checked:
            return
        _, raw = self.command(self.docker + ["info", "--format", "{{json .SecurityOptions}}"])
        options = json.loads(raw)
        if not isinstance(options, list) or not all(isinstance(option, str) for option in options):
            raise RuntimeError("could not validate local Docker security options")
        if any("rootless" in option.lower() or "userns" in option.lower() for option in options):
            raise RuntimeError("only rootful local Docker without user namespace remapping is supported")
        self._daemon_checked = True

    def verify_isolation(self, item: dict) -> dict:
        state = self.inspect_owned(item)
        if state is None:
            raise RuntimeError("owned container disappeared")
        host = state["HostConfig"]
        if (host["NetworkMode"] != item["network"] or host.get("Privileged")
                or host.get("PidMode") not in ("", "private") or host.get("PortBindings")
                or not host.get("ReadonlyRootfs") or state["Config"]["User"] != self.user
                or host.get("NanoCpus") != item["nano_cpus"] or state["Image"] != item["image_id"]):
            raise RuntimeError("owned container isolation or image identity mismatch")
        return state

    def create_args(self, item: dict) -> list[str]:
        self.check_local_daemon()
        return self.docker + ["create", "--pull=never", "--name", item["name"],
            "--label", f"{LABEL}={self.run_id}", "--network", item["network"],
            "--cpus", str(item["nano_cpus"] / 1_000_000_000),
            "--memory", "256m", "--memory-swap", "256m", "--pids-limit", "96",
            "--restart", "no", "--read-only", "--cap-drop", "ALL",
            "--security-opt", "no-new-privileges", "--user", self.user, "--no-healthcheck",
            "--tmpfs", "/tmp:rw,noexec,nosuid,size=32m,mode=1777",
            "--log-driver", "json-file", "--log-opt", "max-size=1m", "--log-opt", "max-file=1"]

    def launch(self, role: str, port: int, peer: int, network: str) -> dict:
        item = {"name": f"library-qbit-{self.run_id}-{role}", "role": role,
                "network": network, "nano_cpus": 200_000_000, "image_id": self.qbit_image_id}
        self.owned.append(item)
        self.record()  # Record the owned name before create, including uncertain CLI outcomes.
        config = self.root / role / "config"
        data = self.root / role / "downloads"
        args = self.create_args(item) + ["--workdir", "/",
            "--mount", f"type=bind,src={config},dst=/config",
            "--mount", f"type=bind,src={data},dst=/downloads",
            # Bypass upstream's root/chown entrypoint; run the known binary directly.
            "--entrypoint", "/bin/busybox", self.qbit_image_id,
            "nice", "-n", "8", "/usr/bin/qbittorrent-nox",
            "--confirm-legal-notice", "--profile=/config",
            f"--webui-port={port}", f"--torrenting-port={peer}"]
        self.command(args)
        self.verify_isolation(item)
        self.command(self.docker + ["start", item["id"]])
        state = self.verify_isolation(item)
        if not state["State"]["Running"]:
            raise RuntimeError("disposable client did not start")
        return item

    def acceptance(self, seed: dict, download: dict) -> None:
        self.verify_isolation(seed)
        self.verify_isolation(download)
        item = {"name": f"library-qbit-{self.run_id}-helper", "role": "helper",
                "network": f"container:{seed['id']}", "nano_cpus": 100_000_000,
                "image_id": self.helper_image_id}
        self.owned.append(item)
        self.record()
        self.command(self.create_args(item) + ["--workdir", str(self.root),
            "--mount", f"type=bind,src={self.root},dst={self.root}",
            "--mount", f"type=bind,src={self.binary},dst=/acceptance-test,readonly",
            "--env", f"HOME={self.root}", "--env", f"LIBRARY_DISPOSABLE_QBITT={OPT_IN}",
            "--env", f"LIBRARY_DISPOSABLE_QBITT_CONFIG={self.root / 'acceptance.json'}",
            "--entrypoint", "/bin/busybox", self.helper_image_id,
            "nice", "-n", "8", "/acceptance-test", "--ignored", "--exact",
            "disposable_owned_qbittorrent", "--test-threads=1", "--nocapture"])
        self.verify_isolation(item)
        # Attached wait is bounded; finally removes only our recorded helper ID on timeout.
        self.command(self.docker + ["start", "--attach", item["id"]], timeout=185)
        state = self.verify_isolation(item)
        if state["State"]["Running"] or state["State"]["ExitCode"] != 0:
            raise RuntimeError("disposable acceptance helper did not exit successfully")

    def run(self) -> None:
        self.qbit_image_id = self.local_image_id(IMAGE)
        self.helper_image_id = self.local_image_id(self.helper_image)
        self.record()
        fixture = Path(__file__).resolve().parent.parent / "tests/fixtures/natural-order.cbz"
        with fixture.open("rb") as stream:
            payload = stream.read(1024 * 1024 + 1)
        if not 0 < len(payload) <= 1024 * 1024:
            raise ValueError("owned CBZ fixture must be between 1 byte and 1 MiB")
        info = encode({b"length": len(payload), b"name": b"real.cbz",
                       b"piece length": 16384, b"private": 1,
                       b"pieces": b"".join(hashlib.sha1(payload[i:i + 16384]).digest()
                                            for i in range(0, len(payload), 16384))})
        torrent = b"d4:info" + info + b"e"
        (self.root / "owned.torrent").write_bytes(torrent)
        config = {"run_id": self.run_id, "infohash": hashlib.sha1(info).hexdigest(),
                  "sha256": hashlib.sha256(payload).hexdigest(), "length": len(payload)}
        for role, port, peer in [("seed", 8081, 6882), ("download", 8080, 6881)]:
            profile = self.root / role / "config" / "qBittorrent" / "config"
            profile.mkdir(parents=True, mode=0o700)
            data = self.root / role / "downloads"
            data.mkdir(mode=0o700)
            password = secrets.token_urlsafe(24)
            self.secrets.append(password)
            config[f"{role}_password"] = password
            (profile / "qBittorrent.conf").write_text(qbit_config(port, peer, password))
            if role == "seed":
                (data / "real.cbz").write_bytes(payload)
            for path in [self.root / role, *(self.root / role).rglob("*")]:
                os.chmod(path, 0o700 if path.is_dir() else 0o600)
        seed = self.launch("seed", 8081, 6882, "none")
        download = self.launch("download", 8080, 6881, f"container:{seed['id']}")
        # Read the namespace from inside the owned seed, never through host /proc/PID.
        self.verify_isolation(seed)
        _, namespace = self.command(self.docker + ["exec", seed["id"], "/bin/busybox",
                                                  "readlink", "/proc/self/ns/net"])
        config["netns"] = namespace.strip()
        if not re.fullmatch(r"net:\[[0-9]+\]", config["netns"]):
            raise RuntimeError("invalid owned seed namespace identity")
        private_json(self.root / "acceptance.json", config)
        self.acceptance(seed, download)
        result = json.loads((self.root / "result.json").read_text())
        if result.get("run_id") != self.run_id or result.get("passed") is not True:
            raise RuntimeError("missing matching acceptance result")
        print("Owned qBittorrent acceptance passed; payload and receipts retained.")

    def cleanup(self) -> bool:
        clean = True
        for item in reversed(self.owned):
            try:
                if self.inspect_owned(item) is None:
                    continue
                try:
                    self.command(self.docker + ["logs", "--tail", "150", item["id"]], check=False)
                except (OSError, subprocess.TimeoutExpired):
                    print("Owned log capture failed; continuing container removal")
                self.command(self.docker + ["rm", "--force", "--volumes", item["id"]], timeout=20)
                item["removed"] = True
                self.record()
            except (RuntimeError, OSError, ValueError, KeyError, subprocess.TimeoutExpired) as error:
                clean = False
                print(f"Owned cleanup incomplete: {type(error).__name__}; see owned-containers.json")
        return clean


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--allow-disposable-queue-writes", action="store_true")
    parser.add_argument("--test-binary", required=True, type=Path)
    parser.add_argument("--helper-image", default="libraryd:local-check",
                        help="existing local Alpine runtime reference; inspected and pinned to its image ID")
    args = parser.parse_args()
    if not args.allow_disposable_queue_writes:
        parser.error("explicit --allow-disposable-queue-writes is required")
    binary = args.test_binary.resolve(strict=True)
    if not binary.is_file() or not os.access(binary, os.X_OK):
        parser.error("--test-binary must be a prebuilt Alpine/musl executable")
    try:
        check_test_binary(binary)
    except (OSError, ValueError) as error:
        parser.error(str(error))
    for program in ("docker",):
        if shutil.which(program) is None:
            parser.error(f"missing prerequisite: {program}")
    os.umask(0o077)
    os.setpriority(os.PRIO_PROCESS, 0, 8)
    harness = Harness(binary, args.helper_image)

    def interrupted(_signum, _frame):
        raise InterruptedError("disposable acceptance interrupted")

    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    passed = False
    try:
        harness.run()
        passed = True
    except (RuntimeError, OSError, ValueError, subprocess.TimeoutExpired) as error:
        print(f"Acceptance failed: {type(error).__name__}: {error}")
    finally:
        # A second signal must not interrupt the bounded owned-ID cleanup.
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        clean = harness.cleanup()
        private_json(harness.root / "harness-result.json", {"passed": passed, "cleanup_complete": clean})
    return 0 if passed and clean else 1


if __name__ == "__main__":
    raise SystemExit(main())
