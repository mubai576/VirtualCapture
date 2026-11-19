"""Cross-check the Rust transport crate against the real C++ wire implementation.

Builds a throwaway generator against proto/vc_wire.h, common/crc32.h and common/fnv1a.h,
writes rust/crates/vc-transport/tests/vectors.txt (with --write) and compares the output
when the fixture already exists. Then runs the crate's Rust tests in an isolated scratch
workspace to keep the cross-language check independent of GPUI and the rest of the
workspace. Finally turns the same C++ packets into a temporary classic pcap under
build/vc-transport-check/run/ and drives the standalone vc-transport CLI (inspect and
replay, including a pcapng rejection) to prove the real diagnostic entry point.

Run: python -I rust/tests/transport_vectors.py [--write] [--skip-cargo]
No captures, NICs, registry or DSHOW operations are performed.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess

ROOT = Path(__file__).resolve().parents[2]
CRATE = ROOT / "rust/crates/vc-transport"
FIXTURE = CRATE / "tests/vectors.txt"
WORK = ROOT / "build/vc-transport-vectors"
SCRATCH = ROOT / "build/vc-transport-check"

GENERATOR = r"""
#include <cstdio>
#include <cstdint>
#include <cstring>
#include <string>
#include <vector>
#include "proto/vc_wire.h"
#include "common/crc32.h"
#include "common/fnv1a.h"

static void hex(const uint8_t* p, size_t n) {
  for (size_t i = 0; i < n; i++) printf("%02x", p[i]);
}

static std::string hex_string(const std::vector<uint8_t>& data) {
  static const char* dig = "0123456789abcdef";
  std::string out;
  out.reserve(data.size() * 2);
  for (uint8_t b : data) { out.push_back(dig[b >> 4]); out.push_back(dig[b & 0xF]); }
  return out;
}

static std::vector<uint8_t> xorshift_bytes(size_t n, uint32_t seed) {
  std::vector<uint8_t> out(n);
  uint32_t s = seed ? seed : 1;
  for (size_t i = 0; i < n; i++) {
    s ^= s << 13; s ^= s >> 17; s ^= s << 5;
    out[i] = (uint8_t)(s & 0xFF);
  }
  return out;
}

static void checksums(const std::string& label, const std::vector<uint8_t>& data) {
  printf("crc32 %s %08x\n", label.c_str(), vc_crc32(data.data(), data.size()));
  printf("fnv64 %s %016llx\n", label.c_str(),
         (unsigned long long)vc_fnv1a64(data.data(), data.size()));
}

int main() {
  printf("# generated from proto/vc_wire.h, common/crc32.h, common/fnv1a.h\n");
  std::vector<uint8_t> empty;
  std::vector<uint8_t> ascii = {'1','2','3','4','5','6','7','8','9'};
  std::vector<uint8_t> ramp(256);
  for (int i = 0; i < 256; i++) ramp[i] = (uint8_t)i;
  checksums("-", empty);
  checksums(hex_string(ascii), ascii);
  checksums(hex_string(ramp), ramp);

  vc_pkt_hdr_t h{};
  memset(&h, 0, sizeof h);
  h.magic = VC_MAGIC; h.frame_id = 0x01020304; h.seq = 5; h.total = 69;
  h.qpc_ns = 0x1122334455667788ull; h.payload_crc = 0xDEADBEEFu;
  h.payload_len = 8958; h.flags = 1; h.pattern = 4;
  printf("header %u %u %u %llu %u %u %u %u ", h.frame_id, h.seq, h.total,
         (unsigned long long)h.qpc_ns, h.payload_crc, h.payload_len, h.flags, h.pattern);
  hex((const uint8_t*)&h, sizeof h); printf("\n");

  // 640x640 default geometry: mtu 9000 and diagnostic mtu 1500.
  size_t b = (size_t)VC_FRAME_W * VC_FRAME_H * 3 / 2;
  size_t p9 = 9000 - 42, p15 = 1500 - 42;
  size_t t9 = (b + p9 - 1) / p9, t15 = (b + p15 - 1) / p15;
  printf("geometry %d %d %zu %d %zu %zu %zu\n", VC_FRAME_W, VC_FRAME_H, b, 9000, p9, t9,
         b - (t9 - 1) * p9);
  printf("geometry %d %d %zu %d %zu %zu %zu\n", VC_FRAME_W, VC_FRAME_H, b, 1500, p15, t15,
         b - (t15 - 1) * p15);

  // 64x64 raw frame, fragmented exactly like the sender (mtu 1500).
  const uint32_t fid = 77;
  std::vector<uint8_t> frame = xorshift_bytes(64 * 64 * 3 / 2, 0x12345678u);
  const size_t pmax = 1458;
  const size_t total = (frame.size() + pmax - 1) / pmax;
  printf("frame %u %zu %016llx\n", fid, frame.size(),
         (unsigned long long)vc_fnv1a64(frame.data(), frame.size()));

  struct Part { uint16_t seq; size_t len; uint32_t crc; const uint8_t* data; };
  std::vector<Part> parts;
  for (size_t s = 0; s < total; s++) {
    size_t off = s * pmax;
    size_t len = (s == total - 1) ? frame.size() - off : pmax;
    parts.push_back(Part{(uint16_t)s, len, vc_crc32(frame.data() + off, len), frame.data() + off});
  }
  auto emit = [&](const char* kind, const Part& p, uint32_t crc) {
    printf("%s %u %u %zu %zu %08x %u %u %llu ", kind, fid, p.seq, total,
           p.len, crc, 0u, 4u, (unsigned long long)424242ull);
    hex(p.data, p.len);
    printf("\n");
  };
  // Deliberately scrambled arrival order with a bad-CRC copy and a duplicate:
  // seq 3, bad seq 0, valid seq 0, seq 4, seq 1, duplicate seq 1, seq 2.
  emit("packet", parts[3], parts[3].crc);
  emit("badcrc", parts[0], parts[0].crc ^ 1u);
  emit("packet", parts[0], parts[0].crc);
  emit("packet", parts[4], parts[4].crc);
  emit("packet", parts[1], parts[1].crc);
  emit("dup", parts[1], parts[1].crc);
  emit("packet", parts[2], parts[2].crc);
  return 0;
}
"""


def find_vcvars():
    vswhere = Path(os.environ.get("ProgramFiles(x86)", r"C:\Program Files (x86)")) / \
        "Microsoft Visual Studio/Installer/vswhere.exe"
    if not vswhere.is_file():
        raise RuntimeError("vswhere.exe not found; install Visual Studio C++ build tools")
    out = subprocess.check_output([
        str(vswhere), "-latest", "-products", "*",
        "-requires", "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
        "-property", "installationPath",
    ], text=True).strip()
    if not out:
        raise RuntimeError("No Visual Studio installation with C++ tools")
    vcvars = Path(out) / "VC/Auxiliary/Build/vcvars64.bat"
    if not vcvars.is_file():
        raise RuntimeError(f"vcvars64.bat not found: {vcvars}")
    return vcvars


def build_and_run():
    WORK.mkdir(parents=True, exist_ok=True)
    source = WORK / "gen.cpp"
    source.write_text(GENERATOR, encoding="utf-8")
    vcvars = find_vcvars()
    # Write a .bat to avoid cmd /c quote-unquoting issues with "Program Files (x86)".
    batch = WORK / "build.bat"
    batch.write_text(
        "@echo off\r\n"
        f'call "{vcvars}" >nul\r\n'
        f'cl /nologo /std:c++17 /O2 /EHsc /I"{ROOT}" "{source}" '
        f'/Fe:"{WORK / "gen.exe"}" /Fo:"{WORK / "gen.obj"}"\r\n',
        encoding="ascii",
        newline="\r\n",
    )
    built = subprocess.run(["cmd", "/c", str(batch)], cwd=WORK, capture_output=True,
                           text=True, encoding="utf-8", errors="replace")
    if built.returncode:
        raise RuntimeError(f"C++ generator build failed:\n{built.stdout}\n{built.stderr}")
    result = subprocess.run([str(WORK / "gen.exe")], cwd=WORK, capture_output=True,
                            text=True, encoding="utf-8", errors="replace")
    if result.returncode:
        raise RuntimeError(f"C++ generator failed:\n{result.stderr}")
    return result.stdout


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
    """Turn the C++ packet lines into real Ethernet frames plus the expected frame hash."""
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
                "<IIHHQIHBB", 0x56434150, int(fid), int(seq), int(total),
                int(qpc_ns), int(crc, 16), int(length), int(flags), int(pattern))
            ethernet = (bytes([0x02, 0, 0, 0, 0, 1]) +      # dst
                        bytes([0x02, 0, 0, 0, 0, 2]) +      # src
                        b"\x88\xb5" + header + payload)
            packets.append((kind, ethernet))
    if expected is None:
        raise RuntimeError("C++ vectors missing frame line")
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
    """Replay the C++ golden packets through the real CLI as a temporary pcap."""
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
    parser.add_argument("--write", action="store_true", help="Rewrite tests/vectors.txt")
    parser.add_argument("--skip-cargo", action="store_true", help="Only cross-check vectors")
    args = parser.parse_args()
    generated = build_and_run()
    if args.write or not FIXTURE.is_file():
        FIXTURE.write_text(generated, encoding="utf-8", newline="\n")
        print(f"wrote {FIXTURE} ({len(generated.splitlines())} lines)")
    else:
        existing = FIXTURE.read_text(encoding="utf-8")
        if existing != generated:
            raise SystemExit("C++ vectors differ from tests/vectors.txt; rerun with --write")
        print("C++ vectors match tests/vectors.txt")
    summary = {"ok": True, "fixture": str(FIXTURE), "generator": str(WORK / "gen.cpp")}
    if not args.skip_cargo:
        manifest = prepare_scratch()
        output = run_rust_tests(manifest)
        print(output.strip().splitlines()[-1] if output.strip() else "rust tests ok")
        summary["cli"] = run_cli_roundtrip(manifest, generated)
        print(f"pcap CLI roundtrip ok: {json.dumps(summary['cli'], ensure_ascii=False)}")
    print(json.dumps(summary, ensure_ascii=False))


if __name__ == "__main__":
    main()
