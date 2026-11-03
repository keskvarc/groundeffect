#!/usr/bin/env python3
"""Summarize bounded strace DDS discovery; mmap extents are not read evidence."""

import argparse
import json
from pathlib import Path
import re


MAX_TRACE_BYTES = 64 * 1024 * 1024
MAX_OPERATIONS = 10000
PREFIX = re.compile(r"^(?:(\d+)\s+)?(\d+\.\d+)\s+(.*)$")
CALL = re.compile(r"^(\w+)\((.*)\)\s+=\s+(-?(?:0x[0-9a-f]+|\d+))(?:\s|$)")
FD = re.compile(r"\d+<([^>]+\.dds)>")


def completed_calls(lines):
    pending = {}
    for line in lines:
        if len(line) > 65536:
            raise ValueError("trace line exceeds byte limit")
        prefix = PREFIX.match(line.strip())
        if not prefix:
            continue
        thread, timestamp, text = prefix.groups()
        if "<unfinished ...>" in text:
            if thread in pending:
                raise ValueError("overlapping unfinished calls in one thread")
            pending[thread] = (timestamp, text.split("<unfinished ...>")[0])
            continue
        if text.startswith("<... "):
            if thread not in pending:
                raise ValueError("resumed syscall lacks its beginning")
            timestamp, start = pending.pop(thread)
            text = start + text.split(" resumed>", 1)[1]
        call = CALL.match(text)
        if call:
            yield thread, float(timestamp), *call.groups()
    if pending:
        raise ValueError("trace ended inside a syscall; finish the capture first")


def arguments(text):
    result, start, depth, quoted, escaped = [], 0, 0, False, False
    for index, character in enumerate(text):
        if quoted:
            if escaped:
                escaped = False
            elif character == "\\":
                escaped = True
            elif character == '"':
                quoted = False
        elif character == '"':
            quoted = True
        elif character in "[({<":
            depth += 1
        elif character in "])}>":
            depth -= 1
        elif character == "," and depth == 0:
            result.append(text[start:index].strip())
            start = index + 1
    if depth or quoted:
        raise ValueError("unbalanced syscall arguments")
    result.append(text[start:].strip())
    return result


def analyze(lines, lengths):
    files, operations, unsupported = {}, [], {}
    for thread, timestamp, operation, text, result in completed_calls(lines):
        match = FD.search(text)
        if not match:
            continue
        name = Path(match.group(1)).name
        if name not in lengths:
            raise ValueError(f"DDS absent from reference manifest: {name}")
        if len(operations) >= MAX_OPERATIONS:
            raise ValueError("DDS operation count exceeds discovery limit")
        fields = arguments(text)
        returned = int(result, 0) if result.startswith("0x") else int(result)
        record = {"thread": thread, "time": timestamp, "file": name,
                  "operation": operation, "result": returned}
        item = files.setdefault(name, {"length": lengths[name], "read_bytes": 0,
                                       "full_file_reads": 0, "mappings": 0,
                                       "unknown_offset_reads": 0, "errors": 0})
        if returned < 0:
            item["errors"] += 1
        elif operation in ("read", "pread64", "readv", "preadv"):
            offset = int(fields[-1]) if operation in ("pread64", "preadv") else None
            if returned == lengths[name]:
                # A regular file cannot return its entire length from a nonzero offset.
                if offset not in (None, 0):
                    raise ValueError("full-file read has a nonzero offset")
                offset = 0
                item["full_file_reads"] += 1
            if offset is not None and offset + returned > lengths[name]:
                raise ValueError("DDS read extends beyond verified file length")
            record.update(offset=offset, returned_bytes=returned)
            if operation in ("read", "pread64"):
                record["requested_bytes"] = int(fields[2])
            else:
                record["requested_bytes"] = sum(map(int, re.findall(r"iov_len=(\d+)", fields[1])))
            if returned > record["requested_bytes"]:
                raise ValueError("read returned more than requested")
            item["read_bytes"] += returned
            item["unknown_offset_reads"] += int(offset is None and returned != 0)
        elif operation == "mmap":
            record.update(mapped_bytes=int(fields[1]), offset=int(fields[5]))
            item["mappings"] += 1
        elif operation not in ("open", "openat", "openat2", "close", "lseek", "munmap"):
            unsupported[operation] = unsupported.get(operation, 0) + 1
        operations.append(record)
    if not files:
        raise ValueError("capture contains no reference DDS operations")
    return {
        "scope": "sampled syscall discovery, not startup/frame latency qualification",
        "ordinary_read_offsets": "unknown unless an entire verified file was returned",
        "mmap_warning": "mapping length does not establish which pages/mips were touched",
        "files": files, "operations": operations, "unsupported_operations": unsupported,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--trace", type=Path, required=True)
    parser.add_argument("--manifest", type=Path, required=True)
    args = parser.parse_args()
    if args.trace.stat().st_size > MAX_TRACE_BYTES:
        parser.error("trace exceeds 64-MiB discovery limit")
    manifest = json.loads(args.manifest.read_text())
    lengths = {}
    for asset in manifest["assets"]:
        if asset["path"].endswith(".dds"):
            name = Path(asset["path"]).name
            if name in lengths:
                parser.error("reference contains ambiguous DDS basenames")
            lengths[name] = asset["bytes"]
    with args.trace.open() as lines:
        print(json.dumps(analyze(lines, lengths), indent=2))


if __name__ == "__main__":
    main()
