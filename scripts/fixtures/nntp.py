#!/usr/bin/env python3
"""Owned loopback-only NNTP fixture. No upstream news access or posting."""
import argparse
import hashlib
import json
from pathlib import Path
import socketserver
import time
import zlib

# Distinct halves make swapped or duplicated segments fail the completed-file hash.
PAYLOAD = bytes(range(256)) * 512 + bytes(reversed(range(256))) * 512
NAME = "owned-payload.bin"
IDS = [f"owned-{part}@fixture.invalid" for part in (1, 2)]


def article(part: int, payload: bytes = PAYLOAD, name: str = NAME) -> bytes:
    split = (len(payload) + 1) // 2
    begin, end = (part - 1) * split, min(part * split, len(payload))
    data = payload[begin:end]
    lines = [f"=ybegin part={part} total=2 line=128 size={len(payload)} name={name}".encode(),
             f"=ypart begin={begin + 1} end={end}".encode()]
    line = bytearray()
    for byte in data:
        byte = (byte + 42) & 255
        encoded = bytes([61, (byte + 64) & 255]) if byte in (0, 9, 10, 13, 32, 46, 61) else bytes([byte])
        if len(line) + len(encoded) > 128:
            lines.append(bytes(line))
            line.clear()
        line.extend(encoded)
    if line:
        lines.append(bytes(line))
    end = f"=yend size={len(data)} part={part} pcrc32={zlib.crc32(data):08x}"
    if part == 2:
        end += f" crc32={zlib.crc32(payload):08x}"
    lines.append(end.encode())
    return b"\r\n".join(lines) + b"\r\n"


def generate(root: Path, payload: bytes = PAYLOAD, name: str = NAME, prefix: str = "owned") -> None:
    ids = [f"{prefix}-{part}@fixture.invalid" for part in (1, 2)]
    segments = "".join(f'<segment bytes="{len(article(n, payload, name))}" number="{n}">{ids[n-1]}</segment>' for n in (1, 2))
    (root / "owned.nzb").write_text(
        '<?xml version="1.0" encoding="UTF-8"?>'
        '<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">'
        f'<file poster="fixture@fixture.invalid" date="1" subject="&quot;{name}&quot; yEnc (1/2)">'
        '<groups><group>alt.test</group></groups><segments>' + segments + '</segments></file></nzb>')


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True
    request_queue_size = 4


class Handler(socketserver.StreamRequestHandler):
    def handle(self):
        self.request.settimeout(185)
        self.wfile.write(b"200 owned fixture ready\r\n")
        for _ in range(100):
            raw = self.rfile.readline(1025)
            if not raw or len(raw) > 1024:
                return
            command, _, argument = raw.decode("ascii", errors="strict").strip().partition(" ")
            command = command.upper()
            if command == "QUIT":
                self.wfile.write(b"205 closing\r\n")
                return
            if command == "CAPABILITIES":
                self.wfile.write(b"101 capabilities\r\nVERSION 2\r\nREADER\r\n.\r\n")
            elif command == "MODE" and argument.upper() == "READER":
                self.wfile.write(b"200 reader\r\n")
            elif command == "GROUP" and argument == "alt.test":
                self.wfile.write(b"211 2 1 2 alt.test\r\n")
            elif command in ("BODY", "ARTICLE", "STAT"):
                ident = argument.strip("<>")
                if ident not in self.server.articles:
                    self.wfile.write(b"430 no such article\r\n")
                    continue
                part, body, name, gate, evidence_root = self.server.articles[ident]
                if command == "STAT":
                    self.wfile.write(f"223 {part} <{ident}>\r\n".encode())
                    continue
                deadline = time.monotonic() + 175
                while not gate.exists():
                    if time.monotonic() >= deadline:
                        self.wfile.write(b"400 fixture gate deadline\r\n")
                        return
                    time.sleep(0.05)
                code = 222 if command == "BODY" else 220
                self.wfile.write(f"{code} {part} <{ident}>\r\n".encode())
                if command == "ARTICLE":
                    self.wfile.write(f"Message-ID: <{ident}>\r\nSubject: {name}\r\n\r\n".encode())
                for line in body.split(b"\r\n")[:-1]:
                    self.wfile.write((b"." if line.startswith(b".") else b"") + line + b"\r\n")
                self.wfile.write(b".\r\n")
                self.wfile.flush()
                with (evidence_root / "served.jsonl").open("a") as log:
                    log.write(json.dumps({"part": part, "sha256": hashlib.sha256(body).hexdigest()}) + "\n")
            else:
                self.wfile.write(b"500 unsupported fixture command\r\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--pipeline", action="store_true")
    args = parser.parse_args()
    with Server(("127.0.0.1", 8119), Handler) as server:
        server.articles = {}
        cases = [(args.root, PAYLOAD, NAME, "owned")]
        if args.pipeline:
            cases = []
            for case, name in [("comic", "comic.cbz"), ("manga", "manga.cbz"), ("magazine", "magazine.pdf")]:
                root = args.root / case
                payload = (root / name).read_bytes()
                if not 2 <= len(payload) <= 1024 * 1024:
                    raise ValueError("invalid owned fixture size")
                cases.append((root, payload, name, case))
        for root, payload, name, prefix in cases:
            for part in (1, 2):
                server.articles[f"{prefix}-{part}@fixture.invalid"] = (
                    part, article(part, payload, name), name, root / "release-articles", root)
        server.serve_forever(poll_interval=0.1)
