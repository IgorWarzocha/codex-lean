"""Read and append replacements to Chromium-pickle ASARs without extracting files."""

import copy
import hashlib
import json
import os
import shutil
import struct
import tempfile
from pathlib import Path


class UnsupportedBundle(ValueError):
    pass


class Asar:
    def __init__(self, path: Path):
        self.path = path
        with path.open("rb") as source:
            stat = os.fstat(source.fileno())
            self.source_signature = self._signature(stat)
            prefix = source.read(16)
            if len(prefix) != 16:
                raise UnsupportedBundle("truncated ASAR prefix")
            size, header_size, payload_size, json_size = struct.unpack("<4I", prefix)
            if (
                size != 4
                or header_size != payload_size + 4
                or header_size != 8 + ((json_size + 3) // 4) * 4
                or header_size > 16 * 1024 * 1024
            ):
                raise UnsupportedBundle("unsupported ASAR pickle header")
            try:
                raw_header = source.read(json_size)
                self.header_sha256 = hashlib.sha256(raw_header).hexdigest()
                self.header = json.loads(raw_header)
            except (ValueError, UnicodeError) as error:
                raise UnsupportedBundle("invalid ASAR JSON header") from error
        self.data_offset = 8 + header_size
        self.data_size = stat.st_size - self.data_offset
        if self.data_size < 0:
            raise UnsupportedBundle("truncated ASAR header")
        self.entries = dict(self._walk(self.header))
        ranges = []
        for name, entry in self.entries.items():
            if "link" in entry or entry.get("unpacked") is True:
                continue
            try:
                offset, length = int(entry["offset"]), entry["size"]
            except (KeyError, TypeError, ValueError) as error:
                raise UnsupportedBundle(f"invalid packed entry: {name}") from error
            if (
                not isinstance(length, int)
                or isinstance(length, bool)
                or offset < 0
                or length < 0
                or offset + length > self.data_size
            ):
                raise UnsupportedBundle(f"out-of-bounds packed entry: {name}")
            if length:
                ranges.append((offset, offset + length))
        ranges.sort()
        # This build deduplicates packed files with identical byte ranges.
        if any(
            left != right and left[1] > right[0]
            for left, right in zip(ranges, ranges[1:])
        ):
            raise UnsupportedBundle("overlapping packed entries")

    @staticmethod
    def _signature(stat):
        return (
            stat.st_dev,
            stat.st_ino,
            stat.st_size,
            stat.st_mtime_ns,
            stat.st_ctime_ns,
        )

    def _check_source(self, source):
        if self._signature(os.fstat(source.fileno())) != self.source_signature:
            raise UnsupportedBundle(
                "input ASAR changed during patching; retry from a pristine copy"
            )

    @classmethod
    def _walk(cls, directory, prefix=""):
        if not isinstance(directory, dict) or not isinstance(
            directory.get("files"), dict
        ):
            raise UnsupportedBundle("invalid ASAR directory")
        for name, entry in directory["files"].items():
            if not name or name in (".", "..") or "/" in name or "\\" in name:
                raise UnsupportedBundle("invalid ASAR entry name")
            if not isinstance(entry, dict):
                raise UnsupportedBundle("invalid ASAR entry")
            path = prefix + name
            if "files" in entry:
                yield from cls._walk(entry, path + "/")
            else:
                yield path, entry

    def read(self, name: str) -> bytes:
        entry = self.entries.get(name)
        if entry is None or "link" in entry or entry.get("unpacked"):
            raise UnsupportedBundle(f"required packed file missing: {name}")
        with self.path.open("rb") as source:
            self._check_source(source)
            source.seek(self.data_offset + int(entry["offset"]))
            data = source.read(entry["size"])
            self._check_source(source)
        if len(data) != entry["size"]:
            raise UnsupportedBundle(f"truncated packed file: {name}")
        return data

    def write(self, output: Path, replacements: dict[str, bytes]) -> None:
        """Keep original payload offsets, links, unpacked entries and metadata intact."""
        header = copy.deepcopy(self.header)
        entries = dict(self._walk(header))
        offset = self.data_size
        for name, data in replacements.items():
            self.read(name)
            entry = entries[name]
            integrity = entry.get("integrity", {})
            block_size = integrity.get("blockSize", 4 * 1024 * 1024)
            if integrity.get("algorithm", "SHA256") != "SHA256" or (
                not isinstance(block_size, int) or block_size <= 0
            ):
                raise UnsupportedBundle(f"unsupported integrity metadata: {name}")
            entry.update(size=len(data), offset=str(offset))
            entry["integrity"] = {
                "algorithm": "SHA256",
                "hash": hashlib.sha256(data).hexdigest(),
                "blockSize": block_size,
                "blocks": [
                    hashlib.sha256(data[start : start + block_size]).hexdigest()
                    for start in range(0, len(data), block_size)
                ],
            }
            offset += len(data)
        encoded = json.dumps(header, ensure_ascii=False, separators=(",", ":")).encode()
        padded = encoded + b"\0" * (-len(encoded) % 4)
        pickle = struct.pack("<4I", 4, 8 + len(padded), 4 + len(padded), len(encoded))
        temporary = None
        try:
            with tempfile.NamedTemporaryFile(dir=output.parent, delete=False) as target:
                temporary = Path(target.name)
                target.write(pickle + padded)
                with self.path.open("rb") as source:
                    self._check_source(source)
                    source.seek(self.data_offset)
                    shutil.copyfileobj(source, target)
                    self._check_source(source)
                for data in replacements.values():
                    target.write(data)
                target.flush()
                os.fsync(target.fileno())
            # Atomic publication without replacing an existing file or symlink.
            os.link(temporary, output)
        finally:
            if temporary is not None:
                temporary.unlink(missing_ok=True)
