#!/usr/bin/env python3
"""Collect third-party license texts for the Rust workspace.

This module is intentionally self-contained (Python standard library only).
It answers the question: "what licenses and notices apply to the crates that
are actually built for the Windows x86_64 target, and where is the real text?"

Usage as a library::

    from pathlib import Path
    from collect_licenses import collect_licenses

    summary = collect_licenses(Path("rust"), Path("rust/target/licenses"))

Usage as a command::

    python rust/packaging/collect_licenses.py rust/target/licenses --root rust

Design notes
------------
* ``cargo metadata --locked --filter-platform x86_64-pc-windows-msvc`` is the
  source of truth for versions, license expressions, repositories and the
  resolved dependency graph.  The graph is traversed from the workspace
  members, so only crates that Cargo resolved for this target are included.
* The script copies every license/copyright/NOTICE/AUTHORS text that the
  upstream crate archive actually ships into ``<destination>/crates/<name>-<version>/``.
  It never rewrites the upstream text.
* If an upstream archive forgets a license file, the script first tries a
  clearly-related donor crate from the same upstream repository/package family
  (for example zune-jpeg 0.5 ships the texts that zune-core 0.4 omits).  The
  donor is recorded in ``PROVENANCE.txt`` and in ``inventory.json``.
* Only copyright-neutral texts (Apache-2.0, MPL-2.0, CC0-1.0, Unicode-3.0,
  etc.) may be copied verbatim from another real license file in the local
  registry.  MIT/BSD/ISC/zlib texts carry attribution in the grant and are
  never synthesized: if no same-project donor exists, the package is reported
  with an explicit exception, and any per-file notices found in upstream
  sources are preserved in ``NOTICE-UPSTREAM.txt``.
* A package is never given a permission text it does not declare without
  recording an explicit exception.  Unresolvable licenses are reported as
  exceptions instead of guessed.
* Explicitly reviewed supplements in ``rust/packaging/licenses/<name>-<version>/``
  may supply a real upstream license text that the crate archive omitted.  Every
  supplemental file records its raw upstream URL, commit SHA and SHA-256.  A
  ``test-only`` exclusion is accepted only for source fragments that are not
  compiled into the release binary and are not a declared license; declared
  licenses can never be excluded.

The JSON summary returned by :func:`collect_licenses` is also written to
``<destination>/inventory.json``; a human-readable aggregate is written to
``<destination>/THIRD-PARTY-NOTICES.md``.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tomllib
from collections import Counter, defaultdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable

GENERATOR = "rust/packaging/collect_licenses.py"
TARGET = "x86_64-pc-windows-msvc"
METADATA_COMMAND = (
    "cargo metadata --locked --format-version 1 --filter-platform " + TARGET
)
SUPPLEMENTAL_LICENSE_ROOT = Path(__file__).resolve().parent / "licenses"

# Cargo allows a few historical, non-SPDX expressions such as
# "MIT/Apache-2.0" and "Apache-2.0 / MIT".  Keep a case-insensitive map for
# the license identifiers we know how to handle.
KNOWN_LICENSE_IDS = {
    "0bsd": "0BSD",
    "apache-2.0": "Apache-2.0",
    "bsd-2-clause": "BSD-2-Clause",
    "bsd-3-clause": "BSD-3-Clause",
    "bsl-1.0": "BSL-1.0",
    "cc0-1.0": "CC0-1.0",
    "cdla-permissive-2.0": "CDLA-Permissive-2.0",
    "isc": "ISC",
    "llvm-exception": "LLVM-exception",
    "mit": "MIT",
    "mit-0": "MIT-0",
    "mpl-2.0": "MPL-2.0",
    "ofl-1.1": "OFL-1.1",
    "unicode-3.0": "Unicode-3.0",
    "unlicense": "Unlicense",
    "zlib": "Zlib",
}

# File-name based detector.  It matches LICENSE, LICENCE, COPYING, COPYRIGHT,
# NOTICE, AUTHORS, UNLICENSE, PATENTS and suffixed forms such as
# LICENSE-MIT, license-apache-2.0, LICENSE.chromium, OFL.txt.
LICENSE_FILE_RE = re.compile(
    r"^(licen[cs]e|copying|copyright|notice|authors?|unlicense|patents?|ofl)"
    r"([._-].*)?$",
    re.IGNORECASE,
)

# Binary/asset extensions are rejected even when the stem looks like a
# license file (for example assets/icons/copyright.svg).
BINARY_SUFFIXES = {
    "a",
    "bin",
    "class",
    "dll",
    "dylib",
    "eot",
    "exe",
    "gif",
    "gz",
    "ico",
    "jar",
    "jpeg",
    "jpg",
    "o",
    "otf",
    "pdf",
    "png",
    "rlib",
    "so",
    "svg",
    "tar",
    "ttc",
    "ttf",
    "webp",
    "woff",
    "woff2",
    "zip",
}

SKIP_DIR_NAMES = {
    ".git",
    ".hg",
    ".svn",
    ".cargo",
    "node_modules",
    "target",
}
MAX_LICENSE_FILE_BYTES = 4 * 1024 * 1024
MAX_WALK_DEPTH = 6
MAX_NOTICE_BLOCK_CHARS = 6000
MAX_NOTICE_FILES = 80
MAX_NOTICE_BLOCKS = 20

# These licenses contain a copyright/attribution line as part of the grant.
# Copying a different package's text would misattribute it, and filling in a
# placeholder would be a fabricated substitute.  Such texts are only accepted
# from the same upstream repository/package family; otherwise they are
# reported as explicit exceptions.
ATTRIBUTION_REQUIRED_IDS = {
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "MIT",
    "MIT-0",
    "Zlib",
}

# Repository families are used for "same upstream project" donor resolution.
_REPO_URL_RE = re.compile(r"^(https?://[^/]+/[^/]+/[^/]+)(?:/.*)?$", re.IGNORECASE)


# ---------------------------------------------------------------------------
# small helpers
# ---------------------------------------------------------------------------


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _read_bytes(path: Path) -> bytes:
    return path.read_bytes()


def _read_text(path: Path) -> str:
    return _read_bytes(path).decode("utf-8", errors="replace")


def _write_text(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "w", encoding="utf-8", newline="\n") as handle:
        handle.write(text)


def _copy_bytes(src: Path, dst: Path) -> None:
    dst.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(src, dst)


def _safe_relative(relative: str) -> Path:
    """Return a safe relative path, rejecting absolute paths and traversal."""
    candidate = Path(relative)
    if candidate.is_absolute() or any(part in ("..", "") for part in candidate.parts):
        raise ValueError(f"unsafe relative path: {relative!r}")
    return candidate


def _sanitize_component(value: str) -> str:
    cleaned = re.sub(r"[^A-Za-z0-9._+-]+", "_", value)
    return cleaned.strip("._") or "unknown"


def _utc_now() -> str:
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def _normalise_newlines(text: str) -> str:
    return text.replace("\r\n", "\n").replace("\r", "\n")


def _find_text_start(text: str, marker: str) -> int:
    return text.lower().find(marker.lower())


def _format_list(values: Iterable[str], limit: int = 8) -> str:
    values = list(values)
    if len(values) <= limit:
        return ", ".join(values)
    return ", ".join(values[:limit]) + f", and {len(values) - limit} more"


# ---------------------------------------------------------------------------
# workspace / cargo metadata
# ---------------------------------------------------------------------------


def _resolve_workspace_root(root: Path) -> Path:
    root = root.resolve()
    if (root / "Cargo.toml").is_file():
        return root
    nested = root / "rust"
    if (nested / "Cargo.toml").is_file():
        return nested
    raise FileNotFoundError(
        f"no Cargo.toml found at {root} or {nested}; pass the Rust workspace root"
    )


def _run_cargo_metadata(workspace_root: Path) -> dict[str, Any]:
    command = [
        "cargo",
        "metadata",
        "--locked",
        "--format-version",
        "1",
        "--filter-platform",
        TARGET,
    ]
    completed = subprocess.run(
        command,
        cwd=str(workspace_root),
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
    )
    if completed.returncode != 0:
        raise RuntimeError(
            f"{' '.join(command)} failed with exit code {completed.returncode}:\n"
            f"{completed.stderr.strip()}"
        )
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"cargo metadata returned invalid JSON: {exc}") from exc


def _active_package_ids(metadata: dict[str, Any]) -> tuple[set[str], set[str]]:
    """Return (workspace_members, reachable_ids).

    Cargo's ``--filter-platform`` already removes non-matching edges from the
    resolve graph.  Walking from the workspace members is still useful because
    it makes the "actually linked" set explicit and robust if Cargo changes
    its filtering behaviour.
    """

    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    members = set(metadata["workspace_members"])
    reachable: set[str] = set()
    stack = list(members)
    while stack:
        package_id = stack.pop()
        if package_id in reachable:
            continue
        reachable.add(package_id)
        node = nodes.get(package_id)
        if not node:
            continue
        for dependency in node.get("deps", []):
            stack.append(dependency["pkg"])
    return members, reachable


def _cargo_home() -> Path:
    cargo_home = os.environ.get("CARGO_HOME")
    if cargo_home:
        return Path(cargo_home)
    return Path.home() / ".cargo"


def _registry_crate_dir(package: dict[str, Any]) -> Path | None:
    """Locate the extracted crate directory.

    ``cargo metadata`` normally points directly at the extracted registry
    source.  The fallback keeps the script usable with unusual CARGO_HOME
    layouts or if the registry source was moved after metadata was produced.
    """

    manifest_path = package.get("manifest_path")
    if manifest_path:
        candidate = Path(manifest_path).parent
        if candidate.is_dir():
            return candidate
    name = package.get("name")
    version = package.get("version")
    if not name or not version:
        return None
    registry_src = _cargo_home() / "registry" / "src"
    if registry_src.is_dir():
        for index_dir in registry_src.iterdir():
            candidate = index_dir / f"{name}-{version}"
            if candidate.is_dir():
                return candidate
    return None


def _manifest_license_file(package: dict[str, Any], crate_dir: Path) -> str | None:
    """Return the Cargo manifest's ``license-file`` value, if any."""

    for filename in ("Cargo.toml", "Cargo.toml.orig"):
        path = crate_dir / filename
        if not path.is_file():
            continue
        try:
            document = tomllib.loads(_read_text(path))
        except (tomllib.TOMLDecodeError, OSError):
            continue
        package_table = document.get("package") or {}
        value = package_table.get("license-file") or package_table.get("license_file")
        if isinstance(value, str) and value.strip():
            return value.strip()
    return None


# ---------------------------------------------------------------------------
# license expression parsing
# ---------------------------------------------------------------------------


def parse_license_expression(expression: str | None) -> dict[str, Any]:
    """Parse a Cargo license expression into normalized identifiers.

    Returns ``{"ids": [...], "exceptions": [...], "unknown": [...]}``.  Cargo
    historically allowed ``/`` as an OR separator; treat it as such.
    """

    result: dict[str, Any] = {"ids": [], "exceptions": [], "unknown": []}
    if not expression or not expression.strip():
        return result

    normalized = expression.replace("/", " OR ")
    tokens = re.findall(r"[A-Za-z0-9][A-Za-z0-9.+-]*|AND|OR|WITH|\(|\)", normalized)
    previous_with = False
    ids: list[str] = []
    exceptions: list[str] = []
    unknown: list[str] = []

    for token in tokens:
        upper = token.upper()
        if token in ("(", ")"):
            previous_with = False
            continue
        if upper in ("AND", "OR"):
            previous_with = False
            continue
        if upper == "WITH":
            previous_with = True
            continue

        canonical = KNOWN_LICENSE_IDS.get(upper.lower())
        if canonical is None:
            # MIT and Apache are the only realistic bare identifiers whose
            # casing needs normalizing; everything else with dots/dashes is
            # treated as an unknown expression component.
            canonical = token
            if canonical not in ids and canonical not in exceptions:
                unknown.append(canonical)
        if previous_with:
            if canonical not in exceptions:
                exceptions.append(canonical)
        else:
            if canonical not in ids:
                ids.append(canonical)
        previous_with = False

    result["ids"] = ids
    result["exceptions"] = exceptions
    result["unknown"] = unknown
    return result


# ---------------------------------------------------------------------------
# license file discovery and classification
# ---------------------------------------------------------------------------


def _is_license_filename(filename: str) -> bool:
    lower = filename.lower()
    if "." in lower:
        stem, suffix = lower.rsplit(".", 1)
        if suffix in BINARY_SUFFIXES:
            return False
        # Dotted version names such as LICENSE-Apache-2.0_WITH_LLVM-exception
        # must be matched as a whole, not split at the version dot.
        base = lower if suffix[:1].isdigit() else stem
    else:
        base = lower
    return bool(LICENSE_FILE_RE.match(base))


def classify_license_text(text: str, filename: str) -> list[str]:
    """Return the canonical license identifiers visible in ``text``."""

    ids: set[str] = set()
    name = filename.lower()
    # License texts are often embedded in `//` or `/* ... */` comments.
    # Remove leading comment markers and collapse whitespace so signature
    # phrases still match when a sentence is split across lines.
    normalized = re.sub(r"(?m)^\s*(?://+|/\*+|\*+|#+)\s?", " ", text)
    normalized = re.sub(r"\s+", " ", normalized).lower()

    # First, content signatures for full license texts.
    if "apache license" in normalized and "version 2.0" in normalized:
        ids.add("Apache-2.0")
    if (
        "llvm exceptions to the apache 2.0 license" in normalized
        or "llvm exception" in normalized
    ):
        ids.add("LLVM-exception")
    if "mozilla public license" in normalized and "2.0" in normalized:
        ids.add("MPL-2.0")
    if "sil open font license" in normalized:
        ids.add("OFL-1.1")
    if "cc0 1.0 universal" in normalized or (
        "creative commons" in normalized and "cc0" in normalized
    ):
        ids.add("CC0-1.0")
    if "unicode license" in normalized and (
        "v3" in normalized or "version 3" in normalized or "v3.0" in normalized
    ):
        ids.add("Unicode-3.0")
    if "community data license agreement" in normalized:
        ids.add("CDLA-Permissive-2.0")
    if "boost software license" in normalized:
        ids.add("BSL-1.0")
    if (
        "this is free and unencumbered software released into the public domain"
        in normalized
    ):
        ids.add("Unlicense")

    if "permission to use, copy, modify, and/or distribute" in normalized:
        if "provided that the above copyright notice" in normalized:
            ids.add("ISC")
        else:
            ids.add("0BSD")

    if "redistribution and use in source and binary forms" in normalized:
        if "neither the name" in normalized:
            ids.add("BSD-3-Clause")
        elif "all advertising materials" in normalized:
            # BSD-4-Clause is not expected in this dependency set, but do not
            # mislabel it as 3-clause if it ever appears.
            ids.add("BSD-4-Clause")
        else:
            ids.add("BSD-2-Clause")

    if "mit no attribution" in normalized:
        ids.add("MIT-0")
    elif "permission is hereby granted, free of charge" in normalized:
        # Unicode and some other licenses contain this phrase but are not MIT.
        if "unicode license" not in normalized and "mozilla public license" not in normalized:
            if "mit license" in normalized or not (
                "permission to use, copy, modify, and/or distribute" in normalized
            ):
                ids.add("MIT")

    if (
        "this software is provided 'as-is'" in normalized
        and "origin of this software must not be misrepresented" in normalized
    ):
        ids.add("Zlib")

    # Filename hints supplement content detection (and help with short files).
    if "apache" in name:
        ids.add("Apache-2.0")
    if "llvm" in name:
        ids.add("LLVM-exception")
    if "mpl" in name:
        ids.add("MPL-2.0")
    if "ofl" in name or "openfont" in name:
        ids.add("OFL-1.1")
    if "cc0" in name:
        ids.add("CC0-1.0")
    if "unicode" in name:
        ids.add("Unicode-3.0")
    if "cdla" in name:
        ids.add("CDLA-Permissive-2.0")
    if "bsl" in name:
        ids.add("BSL-1.0")
    if "unlicense" in name:
        ids.add("Unlicense")
    if "0bsd" in name:
        ids.add("0BSD")
    if "zlib" in name:
        ids.add("Zlib")
    if re.search(r"(^|[-_.])mit[-_.]?0($|[-_.])", name):
        ids.add("MIT-0")
        ids.discard("MIT")
    elif re.search(r"(^|[-_.])mit($|[-_.])", name):
        ids.add("MIT")
    if "bsd-3" in name or "bsd3" in name:
        ids.add("BSD-3-Clause")
    if "bsd-2" in name or "bsd2" in name:
        ids.add("BSD-2-Clause")
    if re.search(r"(^|[-_.])isc($|[-_.])", name):
        ids.add("ISC")
    if re.search(r"(^|[-_.])bsd($|[-_.])", name) and not (
        {"BSD-2-Clause", "BSD-3-Clause", "BSD-4-Clause"} & ids
    ):
        # Ambiguous bare "BSD": leave it to content detection.
        pass

    return sorted(ids)


def _classify_kind(filename: str, ids: list[str]) -> str:
    lower = filename.lower()
    if ids:
        return "license"
    if re.match(r"^(notice|authors?|copyright|patents?)", lower):
        return "notice"
    return "notice"


def _discover_license_files(
    package: dict[str, Any], crate_dir: Path
) -> list[dict[str, Any]]:
    """Find and classify all license-ish files in an extracted crate."""

    records: list[dict[str, Any]] = []
    seen_paths: set[Path] = set()

    def add_record(path: Path, force: bool = False) -> None:
        if path in seen_paths:
            return
        try:
            if not path.is_file():
                return
            stat = path.stat()
        except OSError:
            return
        if not force and not _is_license_filename(path.name):
            return
        if stat.st_size > MAX_LICENSE_FILE_BYTES:
            records.append(
                {
                    "relative_path": path.relative_to(crate_dir).as_posix(),
                    "absolute_path": str(path),
                    "size": stat.st_size,
                    "sha256": None,
                    "license_ids": [],
                    "kind": "oversize-skip",
                    "method": "bundled",
                    "source": None,
                    "error": f"file larger than {MAX_LICENSE_FILE_BYTES} bytes",
                }
            )
            seen_paths.add(path)
            return
        try:
            data = _read_bytes(path)
        except OSError as exc:
            records.append(
                {
                    "relative_path": path.relative_to(crate_dir).as_posix(),
                    "absolute_path": str(path),
                    "size": stat.st_size,
                    "sha256": None,
                    "license_ids": [],
                    "kind": "read-error",
                    "method": "bundled",
                    "source": None,
                    "error": str(exc),
                }
            )
            seen_paths.add(path)
            return
        text = data.decode("utf-8", errors="replace")
        ids = classify_license_text(text, path.name)
        records.append(
            {
                "relative_path": path.relative_to(crate_dir).as_posix(),
                "absolute_path": str(path),
                "size": len(data),
                "sha256": _sha256(data),
                "license_ids": ids,
                "kind": _classify_kind(path.name, ids),
                "method": "bundled",
                "source": None,
                "error": None,
            }
        )
        seen_paths.add(path)

    # Root-level files first (the common case).
    try:
        for entry in sorted(crate_dir.iterdir(), key=lambda p: p.name.lower()):
            if entry.is_file():
                add_record(entry)
    except OSError:
        pass

    # Nested license files (for example ring/third_party/fiat/LICENSE).
    for root, dirnames, filenames in os.walk(crate_dir):
        root_path = Path(root)
        try:
            depth = len(root_path.relative_to(crate_dir).parts)
        except ValueError:
            depth = MAX_WALK_DEPTH
        dirnames[:] = sorted(
            name for name in dirnames if name.lower() not in SKIP_DIR_NAMES
        )
        if depth >= MAX_WALK_DEPTH:
            dirnames[:] = []
        if root_path == crate_dir:
            continue
        for filename in sorted(filenames):
            add_record(root_path / filename)

    # A manifest may point at a license file with a non-standard name.
    manifest_license = _manifest_license_file(package, crate_dir)
    if manifest_license:
        add_record(crate_dir / manifest_license, force=True)

    records.sort(key=lambda record: record["relative_path"].lower())
    return records


# ---------------------------------------------------------------------------
# upstream notices and extra license detection
# ---------------------------------------------------------------------------


def _leading_comment_block(text: str) -> str:
    lines = text.splitlines()
    start: int | None = None
    end = 0
    for index, line in enumerate(lines[:80]):
        stripped = line.strip()
        if not stripped:
            if start is not None:
                end = index + 1
            continue
        if stripped.startswith(("//", "/*", "*", "*/", "#")):
            if start is None:
                start = index
            end = index + 1
        else:
            break
    if start is None:
        return ""
    block = "\n".join(lines[start:end]).strip()
    return block[:MAX_NOTICE_BLOCK_CHARS]


def _readme_license_section(text: str) -> str:
    match = re.search(
        r"(?im)^#{1,6}\s*(?:licen[cs]e|licensing|copyright|attribution)\b.*$",
        text,
    )
    if not match:
        return ""
    tail = text[match.start() :]
    heading_level = len(match.group(0)) - len(match.group(0).lstrip("#"))
    next_heading = re.search(
        rf"(?m)^#{{1,{heading_level}}}\s+\S", tail[len(match.group(0)) :]
    )
    if next_heading:
        tail = tail[: len(match.group(0)) + next_heading.start()]
    return tail.strip()[:MAX_NOTICE_BLOCK_CHARS]


def _notice_blocks(package: dict[str, Any], crate_dir: Path) -> dict[str, Any]:
    """Extract upstream copyright/license notices from README and source headers."""

    blocks: dict[str, tuple[str, str]] = {}

    def add_block(source: str, block: str) -> None:
        block = _normalise_newlines(block).strip()
        if not block:
            return
        low = block.lower()
        if not any(
            key in low
            for key in (
                "copyright",
                "license",
                "licence",
                "spdx",
                "permission is hereby granted",
                "redistribution and use",
                "licensed under",
            )
        ):
            return
        digest = _sha256(block.encode("utf-8"))
        blocks.setdefault(digest, (source, block))

    readme_candidates = [
        crate_dir / "README.md",
        crate_dir / "README",
        crate_dir / "README.txt",
    ]
    for readme in readme_candidates:
        if readme.is_file():
            section = _readme_license_section(_read_text(readme))
            if section:
                add_block(readme.relative_to(crate_dir).as_posix(), section)
            break

    source_files: list[Path] = []
    src_dir = crate_dir / "src"
    if src_dir.is_dir():
        for root, dirnames, filenames in os.walk(src_dir):
            dirnames[:] = sorted(
                name for name in dirnames if name.lower() not in SKIP_DIR_NAMES
            )
            for filename in sorted(filenames):
                if filename.endswith(".rs"):
                    source_files.append(Path(root) / filename)
                    if len(source_files) >= MAX_NOTICE_FILES:
                        break
            if len(source_files) >= MAX_NOTICE_FILES:
                break
    for source in source_files:
        if len(blocks) >= MAX_NOTICE_BLOCKS:
            break
        try:
            text = _read_text(source)
        except OSError:
            continue
        block = _leading_comment_block(text)
        if block:
            add_block(source.relative_to(crate_dir).as_posix(), block)

    return {
        "blocks": [
            {"source": source, "text": block}
            for source, block in sorted(blocks.values(), key=lambda item: item[0])
        ],
        "files_scanned": len(source_files),
    }


def _normalize_notice_text(text: str) -> str:
    """Lowercase and normalize source comments for phrase matching."""

    normalized = re.sub(r"(?m)^\s*(?://+|/\*+|\*+|#+)\s?", " ", text)
    return re.sub(r"\s+", " ", normalized).lower()


def _license_references(text: str) -> list[str]:
    """Return license-file names referenced by a source/README block.

    The regex is deliberately case-sensitive: real references in the crates
    use uppercase ``LICENSE``/``COPYING``.  This avoids matching URL fragments
    such as ``license-image`` or ``license-Apache2.0`` in badges.
    """

    references: list[str] = []
    pattern = re.compile(
        r"(?<!SPDX-)\b(LICEN[CS]E|COPYING)[-._]([A-Za-z][A-Za-z0-9._-]*)"
    )
    for match in pattern.finditer(text):
        suffix = match.group(2)
        if suffix.lower() in ("identifier", "image", "link", "text"):
            continue
        if suffix[0].isdigit():
            continue
        name = match.group(1) + match.group(0)[len(match.group(1)):]
        if name not in references:
            references.append(name)

    # A bare path such as third-party/chromium/LICENSE has no suffix after
    # LICENSE but is still an explicit reference.
    for match in re.finditer(
        r"(?i)([A-Za-z0-9._/-]*chromium[A-Za-z0-9._/-]*[/\\]LICEN[CS]E)\b",
        text,
    ):
        name = match.group(1).replace("\\", "/")
        if name not in references:
            references.append(name)
    return references


def _ids_for_reference(name: str) -> list[str]:
    """Map a referenced license file name to license identifiers.

    Common dual-license references (LICENSE-MIT / LICENSE-APACHE) are not
    mapped here: the crate's own manifest expression already covers those,
    and mapping them would create pointless canonical copies.  We only map
    references that indicate additional, non-obvious licensing.
    """

    low = name.lower()
    if "unicode" in low:
        return ["Unicode-3.0"]
    if "chromium" in low or "bsd-3" in low:
        return ["BSD-3-Clause"]
    if "bsd-2" in low:
        return ["BSD-2-Clause"]
    if "gpl" in low:
        return []
    return []


def _extra_license_ids_from_notices(
    blocks: list[dict[str, str]],
) -> tuple[set[str], list[str], list[dict[str, Any]]]:
    extra: set[str] = set()
    warnings: list[str] = []
    references: list[dict[str, Any]] = []
    seen_warnings: set[str] = set()

    def warn(message: str) -> None:
        if message not in seen_warnings:
            seen_warnings.add(message)
            warnings.append(message)

    for block in blocks:
        normalized = _normalize_notice_text(block["text"])

        # Direct license statements in source headers / README sections.
        if "chromium" in normalized and "bsd" in normalized:
            extra.add("BSD-3-Clause")
        if "bsd-3-clause" in normalized:
            extra.add("BSD-3-Clause")
        if "bsd-2-clause" in normalized or "simplified bsd" in normalized:
            extra.add("BSD-2-Clause")
        if "isc license" in normalized or "isc licence" in normalized:
            extra.add("ISC")
        if "mozilla public license" in normalized:
            extra.add("MPL-2.0")
        if "unicode license" in normalized and (
            "v3" in normalized or "version 3" in normalized
        ):
            extra.add("Unicode-3.0")
        if "cc by 4.0" in normalized or "creative commons attribution 4.0" in normalized:
            warn(
                f"source/README in {block['source']} states that documentation prose "
                "is offered under CC BY 4.0; the referenced LICENSE-DOCS text is not "
                "bundled. This does not affect the compiled binary, but if upstream "
                "documentation is redistributed, preserve the CC BY attribution"
            )
        if (
            "gnu general public license" in normalized
            or "lgpl" in normalized
            or "agpl" in normalized
        ):
            warn(
                f"source notice in {block['source']} mentions a GNU/LGPL/AGPL "
                "license; review manually"
            )

        for name in _license_references(block["text"]):
            ids = _ids_for_reference(name)
            references.append(
                {
                    "source": block["source"],
                    "file": name,
                    "license_ids": ids,
                }
            )
            for license_id in ids:
                extra.add(license_id)
            if "gpl" in name.lower():
                warn(
                    f"source notice in {block['source']} references {name}; "
                    "GNU/LGPL/AGPL text is not bundled and needs manual review"
                )

    return extra, warnings, references


# ---------------------------------------------------------------------------
# canonical license text construction
# ---------------------------------------------------------------------------


def _canonical_text(license_id: str, donor_text: str) -> str:
    """Return a copyright-neutral canonical text from a real license file.

    Only licenses whose canonical text has no package-specific copyright line
    are accepted here.  MIT/BSD/ISC/zlib carry attribution in the grant, so a
    text from an unrelated donor cannot be substituted without misattribution;
    those cases are reported as exceptions instead.

    Apache-2.0 and the LLVM exception can be copied from any real donor because
    their text is identical and contains no copyright holder.  Other
    copyright-neutral licenses (MPL-2.0, CC0-1.0, Unicode-3.0, etc.) are copied
    verbatim from a real license file.
    """

    if license_id in ATTRIBUTION_REQUIRED_IDS:
        raise ValueError(
            f"{license_id} contains an attribution/copyright line; refusing to "
            "synthesize a placeholder substitute"
        )

    text = _normalise_newlines(donor_text).strip() + "\n"

    if license_id == "Apache-2.0":
        terms = _find_text_start(
            text, "TERMS AND CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION"
        )
        if terms < 0:
            raise ValueError("Apache-2.0 donor lacks the standard terms heading")
        start = text.rfind("Apache License", 0, terms)
        if start < 0:
            start = 0
        body = text[start:].strip() + "\n"
        # A donor such as rustix's LICENSE-Apache-2.0_WITH_LLVM-exception
        # appends an extra permission grant.  The requesting package did not
        # declare it, so keep the canonical Apache text only.
        llvm = _find_text_start(body, "LLVM Exceptions to the Apache 2.0 License")
        if llvm >= 0:
            body = body[:llvm].rstrip() + "\n"
        if "end of terms and conditions" not in body.lower():
            raise ValueError("Apache-2.0 donor text is incomplete")
        return body

    if license_id == "LLVM-exception":
        start = _find_text_start(text, "LLVM Exceptions to the Apache 2.0 License")
        if start < 0:
            raise ValueError("LLVM-exception donor lacks the exception heading")
        return text[start:].strip() + "\n"

    # Copyright-neutral license text: use the donor file verbatim.
    return text


# ---------------------------------------------------------------------------
# main collection
# ---------------------------------------------------------------------------


def _prepare_destination(destination: Path) -> None:
    """Create the output directory without clobbering unrelated files.

    A previous run is identified by ``inventory.json`` containing this
    generator's marker.  Only this generator's outputs are removed on refresh;
    native/runtime notices placed in the same directory by the packaging step
    are preserved.  If an unrelated non-empty directory is passed, writing is
    allowed only when none of our output paths already exist.
    """

    output_names = ("crates", "inventory.json", "THIRD-PARTY-NOTICES.md")
    if destination.exists():
        marker = destination / "inventory.json"
        ours = False
        if marker.is_file():
            try:
                previous = json.loads(_read_text(marker))
                ours = previous.get("generated_by") == GENERATOR
            except (json.JSONDecodeError, OSError):
                ours = False
        if ours:
            for name in output_names:
                path = destination / name
                if path.is_dir():
                    shutil.rmtree(path)
                elif path.exists():
                    path.unlink()
        elif any(destination.iterdir()):
            collisions = [name for name in output_names if (destination / name).exists()]
            if collisions:
                raise FileExistsError(
                    f"destination {destination} already contains {', '.join(collisions)} "
                    f"and was not generated by {GENERATOR}; refusing to overwrite"
                )
    destination.mkdir(parents=True, exist_ok=True)


def _package_record(package: dict[str, Any]) -> dict[str, Any]:
    return {
        "name": package.get("name"),
        "version": package.get("version"),
        "license": package.get("license"),
        "repository": package.get("repository"),
        "homepage": package.get("homepage"),
        "authors": list(package.get("authors") or []),
        "manifest_path": package.get("manifest_path"),
        "source": package.get("source"),
        "directory": None,
        "declared_license_ids": [],
        "declared_license_exceptions": [],
        "unknown_license_tokens": [],
        "source_license_ids": [],
        "bundled_license_ids": [],
        "excluded_license_ids": [],
        "exclusion_records": [],
        "files": [],
        "notices": None,
        "license_references": [],
        "warnings": [],
        "exceptions": [],
        "status": "pending",
        "resolution": {
            "bundled": [],
            "same_upstream_family": [],
            "canonical": [],
        },
    }


def _repo_family_key(package: dict[str, Any]) -> str:
    url = package.get("repository") or package.get("homepage") or ""
    if not url:
        return ""
    url = url.strip().lower()
    url = url.split("#", 1)[0].split("?", 1)[0].rstrip("/")
    if url.endswith(".git"):
        url = url[:-4]
    match = _REPO_URL_RE.match(url)
    if match:
        return match.group(1)
    return url


def _name_family_key(package: dict[str, Any]) -> str:
    name = (package.get("name") or "").lower()
    match = re.match(r"([a-z0-9]{4,})[-_]", name)
    return match.group(1) if match else ""


def _same_family(target: dict[str, Any], donor: dict[str, Any]) -> bool:
    if target["name"] == donor["name"]:
        return True
    target_repo = _repo_family_key(target)
    donor_repo = _repo_family_key(donor)
    if target_repo and donor_repo:
        # Repository identity is authoritative.  Do not let a generic name
        # prefix such as "gpui-" match a different project (for example
        # Longbridge's gpui-kit and Zed's gpui-pre).
        return target_repo == donor_repo
    target_prefix = _name_family_key(target)
    donor_prefix = _name_family_key(donor)
    return bool(
        target_prefix and donor_prefix and target_prefix == donor_prefix
    )


def _file_text(record: dict[str, Any]) -> str:
    absolute = record.get("absolute_path")
    if absolute and Path(absolute).is_file():
        return _read_text(Path(absolute))
    return ""


def _choose_donor_file(
    target: dict[str, Any],
    target_record: dict[str, Any],
    license_id: str,
    all_packages: list[dict[str, Any]],
    prefer_family: bool,
) -> tuple[dict[str, Any], dict[str, Any]] | None:
    """Pick a deterministic donor file for ``license_id``.

    Returns ``(donor_package_record, donor_file_record)`` or ``None``.
    """

    candidates: list[tuple[tuple[Any, ...], dict[str, Any], dict[str, Any]]] = []
    target_name = target["name"]
    target_version = target["version"]
    for donor in all_packages:
        if donor is target:
            continue
        if donor["name"] == target_name and donor["version"] == target_version:
            continue
        if not donor.get("directory"):
            continue
        for record in donor["files"]:
            if record.get("kind") != "license":
                continue
            # Only use texts that the donor crate actually shipped.  Do not
            # chain from a previously generated family/canonical file.
            if record.get("method") != "bundled":
                continue
            if license_id not in record.get("license_ids", []):
                continue
            text = _file_text(record)
            if not text.strip():
                continue
            family = _same_family(target, donor) if prefer_family else False
            if prefer_family and not family:
                continue
            same_name = donor["name"] == target_name
            same_repo = bool(
                _repo_family_key(donor)
                and _repo_family_key(donor) == _repo_family_key(target)
            )
            filename = record["relative_path"].lower()
            canonical_name = "license-" + license_id.lower().replace(".", "-")
            canonical_name_hit = canonical_name in filename or (
                license_id.lower() in filename
            )
            no_llvm = not (
                license_id == "Apache-2.0"
                and ("llvm" in filename or "llvm" in text.lower())
            )
            single_id = record.get("license_ids") == [license_id]
            # Family donors are preferred; then a pure canonical-looking file;
            # then stable alphabetical ordering.
            sort_key = (
                0 if family else 1,
                0 if same_name else 1,
                0 if same_repo else 1,
                0 if no_llvm else 1,
                0 if single_id else 1,
                0 if canonical_name_hit else 1,
                donor["name"].lower(),
                donor["version"],
                record["relative_path"].lower(),
            )
            candidates.append((sort_key, donor, record))
    if not candidates:
        return None
    candidates.sort(key=lambda item: item[0])
    _, donor, record = candidates[0]
    return donor, record


def _unique_destination_name(
    crate_output: Path, preferred: str, source_bytes: bytes
) -> str:
    """Return a colliding-free filename, preserving an identical file if present."""

    candidate = crate_output / _safe_relative(preferred)
    if not candidate.exists():
        return preferred
    try:
        if _read_bytes(candidate) == source_bytes:
            return preferred
    except OSError:
        pass
    stem = Path(preferred).name
    parent = Path(preferred).parent
    base = Path(stem).stem
    suffix = "".join(Path(stem).suffixes)
    index = 2
    while True:
        name = f"{base}-{index}{suffix}"
        candidate = crate_output / parent / name
        if not candidate.exists():
            return (parent / name).as_posix()
        try:
            if _read_bytes(candidate) == source_bytes:
                return (parent / name).as_posix()
        except OSError:
            pass
        index += 1


def _load_supplemental(
    package_name: str, version: str
) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    """Load explicitly reviewed supplements for one crate version.

    The only supported layout is
    ``rust/packaging/licenses/<name>-<version>/provenance.json`` plus optional
    ``exclusion.json``.  Each file must have a raw upstream URL, commit SHA and
    matching SHA-256.  Exclusions are deliberately limited to test-only source
    fragments that are not part of the release binary.
    """

    root = SUPPLEMENTAL_LICENSE_ROOT / _sanitize_component(
        f"{package_name}-{version}"
    )
    if not root.is_dir():
        return [], []

    files: list[dict[str, Any]] = []
    provenance_path = root / "provenance.json"
    if provenance_path.is_file():
        document = json.loads(_read_text(provenance_path))
        for item in document.get("files", []):
            relative = _safe_relative(str(item.get("path") or ""))
            source = root / relative
            data = _read_bytes(source)
            digest = _sha256(data)
            expected = str(item.get("sha256") or "")
            if not expected or digest != expected:
                raise ValueError(
                    f"{provenance_path}: SHA-256 mismatch for {relative.as_posix()}"
                )
            url = str(item.get("url") or "")
            commit = str(item.get("commit") or "")
            kind = str(item.get("kind") or "license")
            license_ids = [str(value) for value in item.get("license_ids", [])]
            if kind not in ("license", "notice"):
                raise ValueError(f"{provenance_path}: invalid kind {kind!r}")
            if kind == "license" and (not license_ids or not url or not commit):
                raise ValueError(
                    f"{provenance_path}: license entry {relative.as_posix()} "
                    "needs license_ids, url and commit"
                )
            files.append(
                {
                    "path": relative.as_posix(),
                    "absolute_path": str(source),
                    "data": data,
                    "sha256": digest,
                    "license_ids": license_ids,
                    "kind": kind,
                    "url": url,
                    "commit": commit,
                    "source_sha256": str(item.get("source_sha256") or ""),
                    "source_lines": str(item.get("source_lines") or ""),
                    "note": str(item.get("note") or ""),
                }
            )

    exclusions: list[dict[str, Any]] = []
    exclusion_path = root / "exclusion.json"
    if exclusion_path.is_file():
        document = json.loads(_read_text(exclusion_path))
        for item in document.get("exclusions", []):
            if (
                str(item.get("scope") or "") != "test-only"
                or item.get("release_binary") is not False
            ):
                raise ValueError(
                    f"{exclusion_path}: only release_binary=false test-only "
                    "exclusions are accepted"
                )
            if not item.get("license_id") or not item.get("evidence"):
                raise ValueError(
                    f"{exclusion_path}: exclusion needs license_id and evidence"
                )
            exclusions.append(
                {
                    "license_id": str(item["license_id"]),
                    "scope": "test-only",
                    "path": str(item.get("path") or ""),
                    "evidence": str(item["evidence"]),
                    "url": str(item.get("url") or ""),
                    "commit": str(item.get("commit") or ""),
                    "note": str(item.get("note") or ""),
                }
            )
    return files, exclusions


def _write_provenance(crate_output: Path, entries: list[dict[str, Any]]) -> str | None:
    if not entries:
        return None
    lines = [
        "License text provenance",
        "=======================",
        "",
        "This package archive did not contain a usable text for one or more",
        "declared licenses. The files listed below were resolved without",
        "inventing permission text:",
        "",
    ]
    for entry in entries:
        lines.extend(
            [
                f"* License: {entry['license_id']}",
                f"  File: {entry['file']}",
                f"  Method: {entry['method']}",
            ]
        )
        if entry.get("url"):
            lines.append(f"  Source URL: {entry['url']}")
            lines.append(f"  Source commit: {entry.get('commit', '')}")
            lines.append(f"  File SHA-256: {entry['source_sha256']}")
            if entry.get("upstream_source_sha256"):
                lines.append(
                    f"  Upstream source SHA-256: {entry['upstream_source_sha256']}"
                )
            if entry.get("source_lines"):
                lines.append(f"  Source lines: {entry['source_lines']}")
        else:
            lines.extend(
                [
                    f"  Source: {entry['source_crate']} {entry['source_version']}",
                    f"  Source file: {entry['source_path']}",
                    f"  Source SHA-256: {entry['source_sha256']}",
                ]
            )
        if entry.get("note"):
            lines.append(f"  Note: {entry['note']}")
        lines.append("")
    lines.extend(
        [
            "copyright-neutral texts are reproduced verbatim from a real license",
            "file in the local Cargo registry (Apache-2.0, MPL-2.0, CC0-1.0,",
            "Unicode-3.0, etc.). Attribution-bearing texts (MIT/BSD/ISC/zlib)",
            "are never synthesized; if they could not be obtained from the same",
            "upstream project they are reported as explicit exceptions.",
            "See NOTICE-UPSTREAM.txt for notices found in upstream sources.",
            "",
        ]
    )
    destination = crate_output / "PROVENANCE.txt"
    _write_text(destination, "\n".join(lines))
    return destination.relative_to(crate_output).as_posix()


def _write_upstream_notices(
    crate_output: Path, notices: dict[str, Any] | None
) -> str | None:
    if not notices or not notices.get("blocks"):
        return None
    lines = [
        "Upstream copyright and license notices",
        "=======================================",
        "",
        "Extracted from the crate archive by collect_licenses.py. These notices",
        "are provided for attribution and do not replace the license texts in",
        "this directory. Blocks are copied verbatim from README/source headers.",
        "",
    ]
    for block in notices["blocks"]:
        lines.extend(
            [
                f"---- {block['source']} ----",
                block["text"].rstrip(),
                "",
            ]
        )
    destination = crate_output / "NOTICE-UPSTREAM.txt"
    _write_text(destination, "\n".join(lines))
    return destination.relative_to(crate_output).as_posix()


def collect_licenses(root: Path, destination: Path) -> dict[str, Any]:
    """Collect third-party license texts for the Windows Rust target.

    Parameters
    ----------
    root:
        Rust workspace root (the directory containing ``Cargo.toml`` and
        ``Cargo.lock``).  Passing the repository root is also accepted.
    destination:
        Output directory.  Relative paths are resolved against the ``root``
        argument passed by the caller (so ``root=repo`` plus
        ``destination=rust/target/licenses`` behaves as expected), not against
        the discovered Cargo workspace root.  A previous run is refreshed by
        removing only this script's outputs (``crates/``, ``inventory.json``,
        ``THIRD-PARTY-NOTICES.md``); unrelated files in the same directory are
        preserved.  An unrelated non-empty directory is accepted only when
        none of those output paths already exist.

    Returns
    -------
    dict
        A JSON-serializable summary with counts, per-package expressions and
        versions, copied files, resolution provenance and exceptions.
    """

    original_root = Path(root).resolve()
    workspace_root = _resolve_workspace_root(original_root)
    destination = Path(destination)
    if not destination.is_absolute():
        destination = original_root / destination
    destination = destination.resolve()
    _prepare_destination(destination)

    metadata = _run_cargo_metadata(workspace_root)
    members, reachable = _active_package_ids(metadata)
    by_id = {package["id"]: package for package in metadata["packages"]}

    workspace_packages: list[dict[str, Any]] = []
    third_party: list[dict[str, Any]] = []
    for package_id in sorted(reachable):
        package = by_id[package_id]
        if package_id in members:
            workspace_packages.append(
                {
                    "name": package.get("name"),
                    "version": package.get("version"),
                    "manifest_path": package.get("manifest_path"),
                }
            )
        else:
            third_party.append(_package_record(package))

    # Preserve the raw metadata package for repository/authors lookups.
    package_by_identity = {
        (package.get("name"), package.get("version")): package
        for package in metadata["packages"]
    }

    # First pass: locate and classify every package's own files.
    unavailable: list[dict[str, Any]] = []
    for record in third_party:
        raw = package_by_identity.get((record["name"], record["version"]), {})
        parsed = parse_license_expression(record["license"])
        record["declared_license_ids"] = parsed["ids"]
        record["declared_license_exceptions"] = parsed["exceptions"]
        record["unknown_license_tokens"] = parsed["unknown"]
        if parsed["unknown"]:
            record["exceptions"].append(
                "unknown license expression token(s): " + ", ".join(parsed["unknown"])
            )

        crate_dir = _registry_crate_dir(raw)
        if not crate_dir:
            record["status"] = "unavailable"
            record["exceptions"].append(
                "registry source directory is not available; cannot collect texts"
            )
            unavailable.append(record)
            continue
        record["directory"] = _sanitize_component(f"{record['name']}-{record['version']}")
        record["files"] = _discover_license_files(raw, crate_dir)
        record["notices"] = _notice_blocks(raw, crate_dir)
        source_extra, source_warnings, source_references = (
            _extra_license_ids_from_notices(
                record["notices"]["blocks"] if record["notices"] else []
            )
        )
        record["warnings"].extend(source_warnings)
        record["license_references"] = source_references
        record["source_license_ids"] = sorted(source_extra)
        declared = set(record["declared_license_ids"]) | set(
            record["declared_license_exceptions"]
        )
        for license_id in sorted(source_extra):
            if license_id not in declared:
                record["warnings"].append(
                    f"source/README indicates code or data under {license_id}, "
                    "which is not declared in Cargo.toml"
                )
        record["_crate_dir"] = str(crate_dir)

    # Second pass: copy bundled files and resolve missing texts.
    for record in third_party:
        if record["status"] == "unavailable":
            continue
        crate_dir = Path(record["_crate_dir"])
        crate_output = destination / "crates" / record["directory"]
        crate_output.mkdir(parents=True, exist_ok=True)

        bundled_texts: dict[str, list[str]] = defaultdict(list)
        for file_record in record["files"]:
            if file_record["kind"] in ("oversize-skip", "read-error"):
                record["warnings"].append(
                    f"{file_record['relative_path']}: {file_record.get('error')}"
                )
                continue
            source_path = Path(file_record["absolute_path"])
            relative = _safe_relative(file_record["relative_path"]).as_posix()
            try:
                data = _read_bytes(source_path)
            except OSError as exc:
                record["warnings"].append(
                    f"could not read {source_path}: {exc}"
                )
                continue
            # Guard against case-insensitive filesystem collisions (for
            # example a crate shipping both LICENSE and license).
            destination_name = _unique_destination_name(
                crate_output, relative, data
            )
            target_path = crate_output / _safe_relative(destination_name)
            try:
                _copy_bytes(source_path, target_path)
            except OSError as exc:
                record["warnings"].append(
                    f"could not copy {source_path}: {exc}"
                )
                continue
            file_record["destination"] = destination_name
            for license_id in file_record["license_ids"]:
                bundled_texts[license_id].append(destination_name)
            if file_record["kind"] == "license" and not file_record["license_ids"]:
                record["warnings"].append(
                    f"{destination_name}: license-like file could not be classified"
                )

        supplemental_files, exclusions = _load_supplemental(
            record["name"], record["version"]
        )
        supplemental_provenance: list[dict[str, Any]] = []
        for entry in supplemental_files:
            data = entry["data"]
            destination_name = _unique_destination_name(
                crate_output, entry["path"], data
            )
            target_path = crate_output / _safe_relative(destination_name)
            _copy_bytes(Path(entry["absolute_path"]), target_path)
            record["files"].append(
                {
                    "relative_path": destination_name,
                    "absolute_path": str(target_path),
                    "size": len(data),
                    "sha256": entry["sha256"],
                    "license_ids": entry["license_ids"],
                    "kind": entry["kind"],
                    "method": "explicit-upstream-supplement",
                    "source": entry["url"],
                    "commit": entry["commit"],
                    "error": None,
                    "destination": destination_name,
                }
            )
            if entry["kind"] == "license":
                for license_id in entry["license_ids"]:
                    bundled_texts[license_id].append(destination_name)
            supplemental_provenance.append(
                {
                    "license_id": ", ".join(entry["license_ids"])
                    or "copyright notice",
                    "file": destination_name,
                    "method": "explicit-upstream-supplement",
                    "url": entry["url"],
                    "commit": entry["commit"],
                    "source_sha256": entry["sha256"],
                    "upstream_source_sha256": (
                        entry["source_sha256"]
                        if entry["source_sha256"]
                        and entry["source_sha256"] != entry["sha256"]
                        else ""
                    ),
                    "source_lines": entry["source_lines"],
                    "note": entry["note"],
                }
            )

        if exclusions:
            record["excluded_license_ids"] = sorted(
                {exclusion["license_id"] for exclusion in exclusions}
            )
            record["exclusion_records"] = exclusions
            for exclusion in exclusions:
                record["warnings"].append(
                    f"{exclusion['license_id']} is limited to test-only source "
                    f"{exclusion['path']} and is excluded from release-binary "
                    f"licensing: {exclusion['evidence']}"
                )
            exclusion_name = "TEST-ONLY-EXCLUSIONS.json"
            exclusion_text = (
                json.dumps(
                    {
                        "crate": record["name"],
                        "version": record["version"],
                        "exclusions": exclusions,
                    },
                    indent=2,
                    ensure_ascii=False,
                )
                + "\n"
            )
            _write_text(crate_output / exclusion_name, exclusion_text)
            exclusion_bytes = _read_bytes(crate_output / exclusion_name)
            record["files"].append(
                {
                    "relative_path": exclusion_name,
                    "absolute_path": str(crate_output / exclusion_name),
                    "size": len(exclusion_bytes),
                    "sha256": _sha256(exclusion_bytes),
                    "license_ids": [],
                    "kind": "notice",
                    "method": "explicit-test-only-exclusion",
                    "source": None,
                    "error": None,
                    "destination": exclusion_name,
                }
            )

        record["bundled_license_ids"] = sorted(bundled_texts)

        # License texts required by the manifest expression and by extra
        # notices found in upstream sources (for derived code).
        required: list[str] = []
        for license_id in (
            record["declared_license_ids"]
            + record["declared_license_exceptions"]
            + record["source_license_ids"]
        ):
            if license_id not in required:
                required.append(license_id)

        covered = set(bundled_texts)
        missing = [license_id for license_id in required if license_id not in covered]

        provenance_entries: list[dict[str, Any]] = list(supplemental_provenance)
        resolution = record["resolution"]

        # First fallback: same upstream repository / package family.
        for license_id in list(missing):
            donor = _choose_donor_file(
                record, record, license_id, third_party, prefer_family=True
            )
            if not donor:
                continue
            donor_package, donor_file = donor
            donor_path = Path(donor_file["absolute_path"])
            try:
                data = _read_bytes(donor_path)
            except OSError:
                continue
            preferred = donor_file["relative_path"]
            destination_name = _unique_destination_name(
                crate_output, preferred, data
            )
            target_path = crate_output / _safe_relative(destination_name)
            try:
                _copy_bytes(donor_path, target_path)
            except OSError:
                continue
            entry = {
                "file": destination_name,
                "license_ids": [license_id],
                "kind": "license",
                "method": "same-upstream-family",
                "source": f"{donor_package['name']} {donor_package['version']}:{donor_file['relative_path']}",
                "source_crate": donor_package["name"],
                "source_version": donor_package["version"],
                "source_path": donor_file["relative_path"],
                "source_sha256": donor_file["sha256"],
            }
            record["files"].append(
                {
                    "relative_path": destination_name,
                    "absolute_path": str(target_path),
                    "size": len(data),
                    "sha256": _sha256(data),
                    "license_ids": [license_id],
                    "kind": "license",
                    "method": "same-upstream-family",
                    "source": entry["source"],
                    "error": None,
                    "destination": destination_name,
                }
            )
            provenance_entries.append(
                {
                    "license_id": license_id,
                    "file": destination_name,
                    "method": "same-upstream-family",
                    "source_crate": donor_package["name"],
                    "source_version": donor_package["version"],
                    "source_path": donor_file["relative_path"],
                    "source_sha256": donor_file["sha256"],
                }
            )
            resolution["same_upstream_family"].append(entry)
            covered.add(license_id)
            missing.remove(license_id)

        # Second fallback: canonical text for copyright-neutral licenses,
        # built from a real license file in the local registry.  Licenses that
        # carry copyright attribution are never synthesized; they are left for
        # the explicit exception/warning pass below.
        for license_id in list(missing):
            if license_id in ATTRIBUTION_REQUIRED_IDS:
                continue
            donor = _choose_donor_file(
                record, record, license_id, third_party, prefer_family=False
            )
            if not donor:
                # Leave the decision to the final pass: for an OR expression
                # another alternative may already be covered.
                continue
            donor_package, donor_file = donor
            donor_text = _file_text(donor_file)
            try:
                canonical = _canonical_text(license_id, donor_text)
            except ValueError as exc:
                if license_id in ATTRIBUTION_REQUIRED_IDS:
                    continue
                record["exceptions"].append(
                    f"could not build canonical text for {license_id}: {exc}"
                )
                continue
            data = canonical.encode("utf-8")
            preferred = f"LICENSE-{_sanitize_component(license_id)}.txt"
            destination_name = _unique_destination_name(
                crate_output, preferred, data
            )
            target_path = crate_output / _safe_relative(destination_name)
            _write_text(target_path, canonical)
            entry = {
                "file": destination_name,
                "license_ids": [license_id],
                "kind": "license",
                "method": "canonical-spdx-text",
                "source": f"canonical from {donor_package['name']} {donor_package['version']}:{donor_file['relative_path']}",
                "source_crate": donor_package["name"],
                "source_version": donor_package["version"],
                "source_path": donor_file["relative_path"],
                "source_sha256": donor_file["sha256"],
            }
            record["files"].append(
                {
                    "relative_path": destination_name,
                    "absolute_path": str(target_path),
                    "size": len(data),
                    "sha256": _sha256(data),
                    "license_ids": [license_id],
                    "kind": "license",
                    "method": "canonical-spdx-text",
                    "source": entry["source"],
                    "error": None,
                    "destination": destination_name,
                }
            )
            provenance_entries.append(
                {
                    "license_id": license_id,
                    "file": destination_name,
                    "method": "canonical-spdx-text",
                    "source_crate": donor_package["name"],
                    "source_version": donor_package["version"],
                    "source_path": donor_file["relative_path"],
                    "source_sha256": donor_file["sha256"],
                }
            )
            resolution["canonical"].append(entry)
            covered.add(license_id)
            missing.remove(license_id)

        # Remaining missing licenses are explicit exceptions; never fabricate.
        expression = record.get("license") or ""
        expression_is_or_only = bool(
            re.search(r"\bOR\b|/", expression, re.IGNORECASE)
        ) and not re.search(r"\bAND\b", expression, re.IGNORECASE)
        covered_declared = covered & set(record["declared_license_ids"])
        for license_id in missing:
            if (
                license_id in record.get("excluded_license_ids", [])
                and license_id not in record["declared_license_ids"]
            ):
                continue
            if any(
                f"no license text could be resolved for {license_id}" in item
                or f"could not build canonical text for {license_id}" in item
                for item in record["exceptions"]
            ):
                continue
            if (
                expression_is_or_only
                and license_id in record["declared_license_ids"]
                and covered_declared
            ):
                # A dual/triple-licensed crate is usable as long as one of the
                # offered alternatives is present.  Record the gap without
                # fabricating an attribution-bearing text.
                if license_id in ATTRIBUTION_REQUIRED_IDS:
                    record["warnings"].append(
                        f"license alternative {license_id} has no upstream-bundled "
                        "text and no same-project donor; refusing to substitute a "
                        "placeholder attribution. Another OR alternative is present"
                    )
                else:
                    record["warnings"].append(
                        f"license alternative {license_id} was not bundled and no "
                        "donor text was available; another OR alternative is present"
                    )
                continue
            if license_id in ATTRIBUTION_REQUIRED_IDS:
                record["exceptions"].append(
                    f"no upstream-bundled {license_id} text and no same-project "
                    "donor; refusing to fabricate attribution. Review the upstream "
                    "source notices in NOTICE-UPSTREAM.txt"
                )
            else:
                record["exceptions"].append(
                    f"no license text could be resolved for {license_id}"
                )

        provenance_file = _write_provenance(crate_output, provenance_entries)
        notice_file = _write_upstream_notices(
            crate_output, record.get("notices")
        )
        if notice_file:
            notice_size = (crate_output / notice_file).stat().st_size
            record["files"].append(
                {
                    "relative_path": notice_file,
                    "absolute_path": str(crate_output / notice_file),
                    "size": notice_size,
                    "sha256": _sha256(_read_bytes(crate_output / notice_file)),
                    "license_ids": [],
                    "kind": "notice",
                    "method": "upstream-source-scan",
                    "source": None,
                    "error": None,
                    "destination": notice_file,
                }
            )
        if provenance_file:
            record["files"].append(
                {
                    "relative_path": provenance_file,
                    "absolute_path": str(crate_output / provenance_file),
                    "size": (crate_output / provenance_file).stat().st_size,
                    "sha256": _sha256(_read_bytes(crate_output / provenance_file)),
                    "license_ids": [],
                    "kind": "provenance",
                    "method": "generated",
                    "source": None,
                    "error": None,
                    "destination": provenance_file,
                }
            )

        # Status: bundled-only, resolved, or exception.
        if record["exceptions"]:
            record["status"] = "exception"
        elif resolution["same_upstream_family"] or resolution["canonical"]:
            record["status"] = "resolved"
        else:
            record["status"] = "bundled"
        record["files"].sort(
            key=lambda item: (item["destination"] or item["relative_path"]).lower()
        )
        del record["_crate_dir"]

    # Global exceptions and counts.
    all_files: list[dict[str, Any]] = []
    for record in third_party:
        all_files.extend(record["files"])

    global_exceptions = [
        {
            "package": record["name"],
            "version": record["version"],
            "message": message,
        }
        for record in third_party
        for message in record["exceptions"]
    ]
    global_warnings = [
        {
            "package": record["name"],
            "version": record["version"],
            "message": message,
        }
        for record in third_party
        for message in record["warnings"]
    ]

    expression_counts = Counter(
        record["license"] or "(none)" for record in third_party
    )
    license_id_counts: Counter[str] = Counter()
    for record in third_party:
        for license_id in (
            record["declared_license_ids"]
            + record["declared_license_exceptions"]
            + record["source_license_ids"]
        ):
            license_id_counts[license_id] += 1

    summary: dict[str, Any] = {
        "generated_by": GENERATOR,
        "generated_at": _utc_now(),
        "target": TARGET,
        "metadata_command": METADATA_COMMAND,
        "workspace_root": str(workspace_root),
        "destination": str(destination),
        "cargo_metadata": {
            "package_count": len(metadata.get("packages", [])),
            "workspace_member_count": len(workspace_packages),
            "reachable_package_count": len(reachable),
            "locked": True,
        },
        "counts": {
            "workspace_packages": len(workspace_packages),
            "third_party_packages": len(third_party),
            "packages_bundled": sum(
                1 for record in third_party if record["status"] == "bundled"
            ),
            "packages_resolved": sum(
                1 for record in third_party if record["status"] == "resolved"
            ),
            "packages_with_exceptions": sum(
                1 for record in third_party if record["status"] == "exception"
            ),
            "packages_unavailable": len(unavailable),
            "license_files_copied": sum(
                1
                for record in third_party
                for file_record in record["files"]
                if file_record["kind"] in ("license", "notice")
            ),
            "license_file_records": len(all_files),
            "unique_license_texts": len(
                {
                    file_record["sha256"]
                    for record in third_party
                    for file_record in record["files"]
                    if file_record["kind"] == "license"
                }
            ),
            "exceptions": len(global_exceptions),
            "warnings": len(global_warnings),
        },
        "license_expression_counts": dict(expression_counts.most_common()),
        "license_id_counts": dict(license_id_counts.most_common()),
        "workspace_packages": workspace_packages,
        "packages": [],
        "files": [],
        "exceptions": global_exceptions,
        "warnings": global_warnings,
        "reference_missing_license_files": [],
        "outputs": {
            "inventory": "inventory.json",
            "aggregate_notice": "THIRD-PARTY-NOTICES.md",
            "crate_directory": "crates",
        },
    }

    # Report references to license files that are still not covered after
    # resolution.  Common dual-license references (LICENSE-MIT/LICENSE-APACHE)
    # are intentionally ignored here because their text is normally covered by
    # a combined LICENSE file or a canonical copy.
    for record in third_party:
        references = record.get("license_references") or []
        if not references:
            continue
        covered_ids: set[str] = set(record["declared_license_ids"])
        covered_ids.update(record["declared_license_exceptions"])
        for file_record in record["files"]:
            covered_ids.update(file_record.get("license_ids", []))
        missing_references: list[dict[str, Any]] = []
        warned_reference_names: set[str] = set()
        for reference in references:
            ids = reference.get("license_ids") or []
            interesting = bool(ids) or any(
                key in reference["file"].lower()
                for key in ("chromium", "unicode", "gpl", "lgpl", "agpl")
            )
            if not interesting:
                continue
            if ids and all(license_id in covered_ids for license_id in ids):
                continue
            reference_key = reference["file"].lower()
            if reference_key in warned_reference_names:
                continue
            warned_reference_names.add(reference_key)
            missing_references.append(reference)
            record["warnings"].append(
                f"upstream source references {reference['file']}, but the crate "
                "archive does not contain that file and its text could not be "
                "resolved"
            )
        if missing_references:
            summary["reference_missing_license_files"].append(
                {
                    "package": record["name"],
                    "version": record["version"],
                    "referenced_files": missing_references,
                }
            )

    # Strip private helper fields and publish final package records.
    for record in third_party:
        record["warnings"] = list(dict.fromkeys(record["warnings"]))
        record["exceptions"] = list(dict.fromkeys(record["exceptions"]))
        public_record = {
            key: value
            for key, value in record.items()
            if not key.startswith("_")
        }
        public_record["files"] = [
            {
                "path": file_record.get("destination")
                or file_record["relative_path"],
                "kind": file_record["kind"],
                "method": file_record["method"],
                "license_ids": file_record["license_ids"],
                "size": file_record["size"],
                "sha256": file_record["sha256"],
                "source": file_record["source"],
                "error": file_record["error"],
            }
            for file_record in record["files"]
        ]
        summary["packages"].append(public_record)
        summary["files"].extend(
            f"crates/{record['directory']}/{item['path']}"
            for item in public_record["files"]
        )

    summary["packages"].sort(key=lambda item: (item["name"], item["version"]))

    # Recompute after reference scan and per-package deduplication.
    summary["warnings"] = [
        {
            "package": record["name"],
            "version": record["version"],
            "message": message,
        }
        for record in third_party
        for message in record["warnings"]
    ]
    summary["exceptions"] = [
        {
            "package": record["name"],
            "version": record["version"],
            "message": message,
        }
        for record in third_party
        for message in record["exceptions"]
    ]
    summary["counts"]["warnings"] = len(summary["warnings"])
    summary["counts"]["exceptions"] = len(summary["exceptions"])
    summary["counts"]["packages_with_exceptions"] = sum(
        1 for record in third_party if record["status"] == "exception"
    )

    # Write inventory before the aggregate so a partial run is identifiable.
    inventory_path = destination / "inventory.json"
    _write_text(
        inventory_path,
        json.dumps(summary, indent=2, ensure_ascii=False) + "\n",
    )

    notice_text = _build_aggregate_notice(summary, destination)
    _write_text(destination / "THIRD-PARTY-NOTICES.md", notice_text)

    return summary


# ---------------------------------------------------------------------------
# aggregate notice rendering
# ---------------------------------------------------------------------------


def _load_text_variants(
    summary: dict[str, Any], destination: Path
) -> tuple[dict[str, list[dict[str, Any]]], list[dict[str, Any]]]:
    """Group copied license and notice files by license id and content hash."""

    license_variants: dict[str, dict[str, dict[str, Any]]] = defaultdict(dict)
    notice_variants: dict[str, dict[str, Any]] = {}

    for package in summary["packages"]:
        for file_record in package["files"]:
            if file_record["kind"] not in ("license", "notice"):
                continue
            path = destination / "crates" / package["directory"] / file_record["path"]
            try:
                data = _read_bytes(path)
            except OSError:
                continue
            digest = file_record["sha256"] or _sha256(data)
            text = data.decode("utf-8", errors="replace")
            label = f"{package['name']} {package['version']}"
            if file_record["kind"] == "license" and file_record["license_ids"]:
                for license_id in file_record["license_ids"]:
                    bucket = license_variants[license_id]
                    entry = bucket.setdefault(
                        digest,
                        {
                            "digest": digest,
                            "text": text,
                            "packages": set(),
                            "paths": set(),
                        },
                    )
                    entry["packages"].add(label)
                    entry["paths"].add(file_record["path"])
            elif file_record["kind"] == "notice":
                entry = notice_variants.setdefault(
                    digest,
                    {
                        "digest": digest,
                        "text": text,
                        "packages": set(),
                        "paths": set(),
                    },
                )
                entry["packages"].add(label)
                entry["paths"].add(file_record["path"])

    def finalize(buckets: dict[str, dict[str, Any]]) -> list[dict[str, Any]]:
        result = list(buckets.values())
        for entry in result:
            entry["packages"] = sorted(entry["packages"])
            entry["paths"] = sorted(entry["paths"])
        result.sort(
            key=lambda entry: (
                -len(entry["packages"]),
                entry["digest"],
            )
        )
        return result

    license_result = {
        license_id: finalize(buckets)
        for license_id, buckets in sorted(license_variants.items())
    }
    return license_result, finalize(notice_variants)


def _fenced(text: str) -> str:
    return "````text\n" + _normalise_newlines(text).strip() + "\n````"


def _build_aggregate_notice(summary: dict[str, Any], destination: Path) -> str:
    license_variants, notice_variants = _load_text_variants(summary, destination)

    lines: list[str] = [
        "# Third-Party License Notices",
        "",
        f"Generated by `{GENERATOR}` at {summary['generated_at']}.",
        f"Target: `{summary['target']}` (Windows x86_64).",
        f"Workspace: `{summary['workspace_root']}`.",
        "",
        "This file is generated from the extracted crate archives in the local",
        "Cargo registry. It reports the licenses declared by each crate, the",
        "actual license texts copied next to this file, and any exceptions where",
        "upstream did not ship a usable text.",
        "",
        "> This is a technical inventory, not legal advice. The original texts in",
        "> `crates/` are the authoritative copies; they were not rewritten.",
        "",
        "## Summary",
        "",
        f"- Third-party crates resolved for this target: "
        f"{summary['counts']['third_party_packages']}",
        f"- Crates with bundled texts only: {summary['counts']['packages_bundled']}",
        f"- Crates with texts resolved from an upstream family: "
        f"{sum(1 for p in summary['packages'] if p['resolution']['same_upstream_family'])}",
        f"- Crates with canonical fallback texts: "
        f"{sum(1 for p in summary['packages'] if p['resolution']['canonical'])}",
        f"- Crates with exceptions: {summary['counts']['packages_with_exceptions']}",
        f"- License/notice file records: {summary['counts']['license_file_records']}",
        f"- Unique license texts: {summary['counts']['unique_license_texts']}",
        f"- Warnings: {summary['counts']['warnings']}",
        "",
        "## License expressions",
        "",
        "| Expression | Crates |",
        "| --- | ---: |",
    ]
    for expression, count in summary["license_expression_counts"].items():
        lines.append(f"| `{expression}` | {count} |")

    lines.extend(
        [
            "",
            "## Package inventory",
            "",
            "Resolution codes: `bundled` = texts shipped by the crate; "
            "`resolved` = some texts came from a same-project donor or a "
            "canonical fallback; `exception` = a required text could not be "
            "resolved without guessing.",
            "",
            "| Crate | Version | Declared license | Resolution | Files |",
            "| --- | --- | --- | --- | --- |",
        ]
    )
    for package in summary["packages"]:
        files = "<br>".join(
            f"`{item['path']}`"
            for item in package["files"]
            if item["kind"] in ("license", "notice", "provenance")
        )
        if not files:
            files = "(none)"
        declared = (package["license"] or "(none)").replace("|", "\\|")
        lines.append(
            f"| {package['name']} | {package['version']} | `{declared}` | "
            f"{package['status']} | {files} |"
        )

    lines.extend(
        [
            "",
            "## License texts",
            "",
            "Texts are deduplicated by content. A variant used by several crates",
            "is shown once. Per-crate copies are under `crates/<name>-<version>/`.",
            "",
        ]
    )
    for license_id in sorted(license_variants):
        variants = license_variants[license_id]
        packages = sorted({pkg for variant in variants for pkg in variant["packages"]})
        lines.extend(
            [
                f"### {license_id}",
                "",
                f"Used by {len(packages)} crate(s); {len(variants)} distinct text "
                "variant(s).",
                "",
            ]
        )
        for index, variant in enumerate(variants, 1):
            lines.extend(
                [
                    f"<details><summary>Variant {index} — used by "
                    f"{_format_list(variant['packages'], 6)}</summary>",
                    "",
                    _fenced(variant["text"]),
                    "",
                    "</details>",
                    "",
                ]
            )

    if notice_variants:
        lines.extend(
            [
                "## Notices, copyright and attribution",
                "",
                "These blocks are NOTICE/COPYRIGHT/AUTHORS texts and notices",
                "extracted from upstream sources. They are attribution material,",
                "not replacements for the license texts above.",
                "",
            ]
        )
        for index, variant in enumerate(notice_variants, 1):
            lines.extend(
                [
                    f"<details><summary>Notice {index} — used by "
                    f"{_format_list(variant['packages'], 6)}</summary>",
                    "",
                    _fenced(variant["text"]),
                    "",
                    "</details>",
                    "",
                ]
            )

    if summary["exceptions"]:
        lines.extend(
            [
                "## Exceptions",
                "",
                "The following license texts could not be resolved without",
                "inventing permission text. Review these manually before release.",
                "",
            ]
        )
        for item in summary["exceptions"]:
            lines.append(
                f"- `{item['package']} {item['version']}`: {item['message']}"
            )
        lines.append("")

    if summary["warnings"]:
        lines.extend(
            [
                "## Warnings",
                "",
                "Non-fatal findings that deserve review (for example a missing",
                "license file referenced by an upstream source header).",
                "",
            ]
        )
        for item in summary["warnings"]:
            lines.append(
                f"- `{item['package']} {item['version']}`: {item['message']}"
            )
        lines.append("")

    lines.extend(
        [
            "## Method",
            "",
            "1. `cargo metadata --locked --filter-platform x86_64-pc-windows-msvc`",
            "   defines the dependency graph.",
            "2. Every license/copyright/NOTICE/AUTHORS text in each extracted",
            "   crate archive is copied to `crates/<name>-<version>/`.",
            "3. Missing texts are first resolved from the same upstream repository",
            "   or package family; the donor and SHA-256 are recorded in",
            "   `PROVENANCE.txt` and `inventory.json`.",
            "4. If no donor exists, a copyright-neutral text (Apache-2.0,",
            "   MPL-2.0, CC0-1.0, Unicode-3.0, etc.) may be copied verbatim from",
            "   a real license file in the local registry. Attribution-bearing",
            "   texts (MIT/BSD/ISC/zlib) are never synthesized.",
            "5. Remaining gaps, including missing MIT/BSD/ISC attribution, are",
            "   reported as explicit exceptions rather than filled with invented",
            "   permission text or placeholder copyright lines.",
            "",
            "See `inventory.json` for the machine-readable record.",
            "",
        ]
    )
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Collect third-party license notices for the Windows x86_64 Rust "
            "workspace dependencies."
        )
    )
    parser.add_argument(
        "destination",
        help="output directory (for example rust/target/licenses)",
    )
    parser.add_argument(
        "--root",
        default=".",
        help=(
            "Rust workspace root or repository root (default: current "
            "directory; a repository root containing rust/Cargo.toml is accepted)"
        ),
    )
    args = parser.parse_args(argv)

    try:
        summary = collect_licenses(Path(args.root), Path(args.destination))
    except (FileNotFoundError, FileExistsError, RuntimeError) as exc:
        print(f"collect_licenses: {exc}", file=sys.stderr)
        return 2

    print(json.dumps(
        {
            "destination": summary["destination"],
            "counts": summary["counts"],
            "license_expression_counts": summary["license_expression_counts"],
        },
        indent=2,
        ensure_ascii=False,
    ))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
