"""Regenerate and cross-check the frozen vc-transport golden vectors in Python.

The oracle is standard-library-only (integer xorshift32, `zlib.crc32` and the
independent `reference/fnv1a.py`) and needs no C++ compiler, headers or legacy
tools.  `rust/crates/vc-transport/tests/vectors.txt` is frozen: by default this
script regenerates the expected text and requires an exact match, and it
refuses to overwrite an existing fixture that differs.  `--write` only creates
a missing fixture (and leaves a matching one untouched).

With cargo enabled it then runs the crate's Rust tests in an isolated scratch
workspace, turns the same packets into a temporary classic pcap under
build/vc-transport-check/run/, and drives the standalone vc-transport CLI
(inspect and replay, including a pcapng rejection).  `--skip-cargo` performs
only the pure-Python vector comparison.

Run: python -I rust/tests/transport_vectors.py [--write] [--skip-cargo]
No captures, NICs, registry or DSHOW operations are performed.
"""
import argparse
import json
import struct
import subprocess
import sys
from pathlib import Path
import shutil
import zlib

ROOT = Path(__file__).resolve().parents[2]
CRATE = ROOT / "rust/crates/vc-transport"
FIXTURE = CRATE / "tests/vectors.txt"
SCRATCH = ROOT / "build/vc-transport-check"

REFERENCE_DIR = Path(__file__).resolve().parent / "reference"
if str(REFERENCE_DIR) not in sys.path:
    sys.path.insert(0, str(REFERENCE_DIR))
from fnv1a import fnv1a64  # noqa: E402

# Frozen wire contract (proto/vc_wire.h): 28-byte little-endian packed header.
VC_MAGIC = 0x56434150
VC_FRAME_W = 640
VC_FRAME_H = 640
PACKET_OVERHEAD = 42  # 14-byte Ethernet + 28-byte VC header


def xorshift_bytes(count, seed):
    """xorshift32 stream, one low byte per iteration, uint32 wrapping."""
    state = seed & 0xFFFFFFFF or 1
    out = bytearray(count)
    for index in range(count):
        state ^= (state << 13) & 0xFFFFFFFF
        state ^= state >> 17
        state ^= (state << 5) & 0xFFFFFFFF
        state &= 0xFFFFFFFF
        out[index] = state & 0xFF
    return bytes(out)


def crc32(data):
    return zlib.crc32(data) & 0xFFFFFFFF


def hex_string(data):
    return data.hex()


def checksums(label, data):
    return [
        "crc32 %s %08x" % (label, crc32(data)),
        "fnv64 %s %016x" % (label, fnv1a64(data)),
    ]


def generate():
    """Build the exact frozen vector text without any C++ involvement."""
    lines = ["# generated from proto/vc_wire.h, common/crc32.h, common/fnv1a.h"]
    lines += checksums("-", b"")
    lines += checksums(hex_string(b"123456789"), b"123456789")
    lines += checksums(hex_string(bytes(range(256))), bytes(range(256)))

    header = struct.pack(
        "<IIHHQIHBB", VC_MAGIC, 0x01020304, 5, 69,
        0x1122334455667788, 0xDEADBEEF, 8958, 1, 4)
    lines.append("header 16909060 5 69 1234605616436508552 3735928559 8958 1 4 "
                 + header.hex())

    frame_bytes = VC_FRAME_W * VC_FRAME_H * 3 // 2
    for mtu in (9000, 1500):
        payload_max = mtu - PACKET_OVERHEAD
        total = (frame_bytes + payload_max - 1) // payload_max
        tail = frame_bytes - (total - 1) * payload_max
        lines.append("geometry %d %d %d %d %d %d %d"
                     % (VC_FRAME_W, VC_FRAME_H, frame_bytes, mtu,
                        payload_max, total, tail))

    frame_id = 77
    frame = xorshift_bytes(64 * 64 * 3 // 2, 0x12345678)
    payload_max = 1458
    total = (len(frame) + payload_max - 1) // payload_max
    lines.append("frame %d %d %016x" % (frame_id, len(frame), fnv1a64(frame)))

    parts = []
    for seq in range(total):
        offset = seq * payload_max
        length = len(frame) - offset if seq == total - 1 else payload_max
        part = frame[offset:offset + length]
        parts.append((seq, length, crc32(part), part))

    def emit(kind, part, crc):
        seq, length, _part_crc, payload = part
        lines.append("%s %d %d %d %d %08x %d %d %d %s"
                     % (kind, frame_id, seq, total, length, crc, 0, 4, 424242,
                        payload.hex()))

    # Deliberately scrambled arrival order with a bad-CRC copy and a duplicate:
    # seq 3, bad seq 0, valid seq 0, seq 4, seq 1, duplicate seq 1, seq 2.
    emit("packet", parts[3], parts[3][2])
    emit("badcrc", parts[0], parts[0][2] ^ 1)
    emit("packet", parts[0], parts[0][2])
    emit("packet", parts[4], parts[4][2])
    emit("packet", parts[1], parts[1][2])
    emit("dup", parts[1], parts[1][2])
    emit("packet", parts[2], parts[2][2])
    return "\n".join(lines) + "\n"


def check_fixture(generated, write):
    if not FIXTURE.is_file():
        if not write:
            raise SystemExit(f"missing frozen fixture {FIXTURE}; rerun with --write")
        FIXTURE.write_text(generated, encoding="utf-8", newline="\n")
        print(f"wrote {FIXTURE} ({len(generated.splitlines())} lines)")
        return
    existing = FIXTURE.read_text(encoding="utf-8")
    if existing != generated:
        raise SystemExit(
            "Python vectors differ from the frozen rust/crates/vc-transport/tests/"
            "vectors.txt; refusing to alter it")
    print("Python vectors match rust/crates/vc-transport/tests/vectors.txt")


def prepare_scratch():
    if SCRATCH.exists():
        shutil.rmtree(SCRATCH)
    SCRATCH.mkdir(parents=True)
    shutil.copytree(CRATE, SCRATCH / "vc-transport")
    manifest = SCRATCH / "vc-transport/Cargo.toml"
    manifest.write_text(manifest.read_text(encoding="utf-8") + "\n[workspace]\n",
                        encoding="utf-8")
    return manifest


def run_rust_tests(manifest):
    result = subprocess.run(
        ["cargo", "test", "--manifest-path", str(manifest), "--quiet"],
        cwd=ROOT, capture_output=True, text=True, encoding="utf-8", errors="replace")
    if result.returncode:
        raise RuntimeError(f"Rust tests failed:\n{result.stdout}\n{result.stderr}")
    return result.stdout


def golden_packets(generated):
    """Turn the packet lines into real Ethernet frames plus the expected frame hash."""
    packets = []
    expected = None
    for line in generated.splitlines():
        fields = line.split()
        if not fields:
            continue
        if fields[0] == "frame":
            expected = {"frame_id": int(fields[1]), "bytes": int(fields[2]),
                        "hash": fields[3]}
        elif fields[0] in ("packet", "badcrc", "dup"):
            kind, fid, seq, total, length, crc, flags, pattern, qpc_ns = fields[:9]
            payload = bytes.fromhex(fields[9])
            if len(payload) != int(length):
                raise RuntimeError(f"vector length mismatch: {line[:40]}")
            header = struct.pack(
                "<IIHHQIHBB", VC_MAGIC, int(fid), int(seq), int(total),
                int(qpc_ns), int(crc, 16), int(length), int(flags), int(pattern))
            ethernet = (bytes([0x02, 0, 0, 0, 0, 1]) +      # dst
                        bytes([0x02, 0, 0, 0, 0, 2]) +      # src
                        b"\x88\xb5" + header + payload)
            packets.append((kind, ethernet))
    if expected is None:
        raise RuntimeError("vectors missing frame line")
    return packets, expected


def write_pcap(path, packets):
    """Classic little-endian, microsecond pcap with LINKTYPE_ETHERNET."""
    with open(path, "wb") as out:
        out.write(struct.pack("<IHHiIII", 0xA1B2C3D4, 2, 4, 0, 0, 65535, 1))
        for index, (_, frame) in enumerate(packets):
            out.write(struct.pack("<IIII", 1_700_000_000 + index // 1000,
                                  (index % 1000) * 1000, len(frame), len(frame)))
            out.write(frame)


def cargo_cli(manifest, args, expect_failure=False):
    result = subprocess.run(
        ["cargo", "run", "--quiet", "--manifest-path", str(manifest),
         "--bin", "vc-transport", "--", *args],
        cwd=ROOT, capture_output=True, text=True, encoding="utf-8", errors="replace")
    if expect_failure:
        if result.returncode == 0:
            raise RuntimeError(f"vc-transport unexpectedly succeeded: {args}")
    elif result.returncode:
        raise RuntimeError(
            f"vc-transport failed ({args}):\n{result.stdout}\n{result.stderr}")
    return result


def run_cli_roundtrip(manifest, generated):
    """Replay the golden packets through the real CLI as a temporary pcap."""
    packets, expected = golden_packets(generated)
    run_dir = SCRATCH / "run"
    run_dir.mkdir(parents=True, exist_ok=True)
    pcap = run_dir / "transport-golden.pcap"
    write_pcap(pcap, packets)

    replay = cargo_cli(manifest, [
        "replay", str(pcap), "--width", "64", "--height", "64", "--mtu", "1500",
        "--source-mac", "02:00:00:00:00:02", "--json", "--framewise",
    ])
    lines = [line for line in replay.stdout.splitlines() if line.strip()]
    if len(lines) != 2:
        raise RuntimeError(f"expected one frame line + summary, got {lines!r}")
    frame = json.loads(lines[0])
    summary = json.loads(lines[1])
    if frame["frame"] != expected["frame_id"] or frame["hash"] != expected["hash"]:
        raise RuntimeError(f"replayed frame mismatch: {frame} vs {expected}")
    if frame["bytes"] != expected["bytes"]:
        raise RuntimeError(f"replayed size mismatch: {frame} vs {expected}")
    if summary["packets_read"] != len(packets):
        raise RuntimeError(f"packets_read {summary['packets_read']} != {len(packets)}")
    stats = summary["stats"]
    want = {"complete": 1, "packets_recv": len(packets) - 1, "packets_lost": 1,
            "crc_errors": 1, "duplicate": 1, "dropped": 0, "late": 0}
    for key, value in want.items():
        if stats[key] != value:
            raise RuntimeError(f"replay stats {key}={stats[key]}, expected {value}")
    if summary["ignored"] != {"BadCrc": 1, "Duplicate": 1}:
        raise RuntimeError(f"unexpected ignored reasons: {summary['ignored']}")
    if summary["config"]["total"] != 5 or summary["config"]["payload_max"] != 1458:
        raise RuntimeError(f"unexpected session config: {summary['config']}")
    if summary["inflight_at_end"] != 0:
        raise RuntimeError("reassembler left inflight frames")
    if summary["rseg"]["samples"] != 1:
        raise RuntimeError("missing rseg sample")

    inspect = cargo_cli(manifest, ["inspect", str(pcap), "--json"])
    info = json.loads(inspect.stdout.strip().splitlines()[-1])
    if info["packets"] != len(packets) or info["vc_packets"] != len(packets):
        raise RuntimeError(f"inspect counts mismatch: {info}")
    if info["source_macs"] != ["02:00:00:00:00:02"]:
        raise RuntimeError(f"inspect source MACs mismatch: {info['source_macs']}")
    if info["vc_totals"] != [5] or info["vc_bad_magic"] != 0:
        raise RuntimeError(f"inspect VC header summary mismatch: {info}")
    if info["format"]["endian"] != "little" or info["format"]["timestamp"] != "microsecond":
        raise RuntimeError(f"inspect format mismatch: {info['format']}")

    pcapng = run_dir / "transport-reject.pcapng"
    pcapng.write_bytes(b"\x0a\x0d\x0d\x0a" + b"\x00" * 12)
    rejected = cargo_cli(manifest, ["inspect", str(pcapng)], expect_failure=True)
    if "pcapng" not in rejected.stderr.lower():
        raise RuntimeError(f"pcapng rejection not reported: {rejected.stderr}")

    return {
        "pcap": str(pcap),
        "packets": len(packets),
        "frame": frame["frame"],
        "hash": frame["hash"],
        "complete": stats["complete"],
        "crc_errors": stats["crc_errors"],
        "duplicate": stats["duplicate"],
        "pcapng_rejected": True,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--write", action="store_true",
                        help="create tests/vectors.txt only if it is missing; never "
                             "overwrite a differing frozen fixture")
    parser.add_argument("--skip-cargo", action="store_true",
                        help="only regenerate/compare the Python vectors")
    args = parser.parse_args()
    generated = generate()
    check_fixture(generated, args.write)
    summary = {"ok": True, "fixture": str(FIXTURE), "oracle": "python-stdlib"}
    if not args.skip_cargo:
        manifest = prepare_scratch()
        output = run_rust_tests(manifest)
        print(output.strip().splitlines()[-1] if output.strip() else "rust tests ok")
        summary["cli"] = run_cli_roundtrip(manifest, generated)
        print(f"pcap CLI roundtrip ok: {json.dumps(summary['cli'], ensure_ascii=False)}")
    print(json.dumps(summary, ensure_ascii=False))


if __name__ == "__main__":
    main()
