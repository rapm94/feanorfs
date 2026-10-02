#!/usr/bin/env python3
"""Deterministic stand-in for a coding agent, run under `feanorfs agent run`.

It follows the loop a real agent is taught: `agent claim` the paths it will
edit (one blocking call), check each write with `agent guard`, edit, then
`agent done` (one call that waits for the edits to land and finishes the
task). It prints one JSON line with its own counters on exit.
"""

import json
import os
import subprocess
import sys
import time

POLL_SECONDS = 0.5
CLAIM_PENDING = 3


def ff(*args, check=True):
    result = subprocess.run(
        [os.environ["FEANORFS_EVAL_BIN"], *args],
        capture_output=True,
        text=True,
    )
    if check and result.returncode != 0:
        raise SystemExit(f"feanorfs {' '.join(args)} failed: {result.stderr.strip()}")
    return result


def apply_edit(edit):
    path = edit["path"]
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    if "write" in edit:
        text = edit["write"]
    else:
        with open(path, encoding="utf-8") as handle:
            lines = handle.read().splitlines(keepends=True)
        anchor = edit["insert_after"]
        index = next(i for i, line in enumerate(lines) if line.rstrip("\n") == anchor)
        lines.insert(index + 1, edit["text"] + "\n")
        text = "".join(lines)
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(text)


def main():
    scenario = json.load(open(sys.argv[1], encoding="utf-8"))
    name = os.environ["FEANORFS_AGENT"]
    agent = next(a for a in scenario["agents"] if a["name"] == name)
    timeout = float(os.environ.get("FEANORFS_EVAL_STEP_TIMEOUT", "120"))
    counters = {"agent": name, "guard_blocks": 0, "tokens": 0}

    deadline = time.monotonic() + timeout
    while True:
        claim = ff("agent", "claim", *agent["scope"], "--timeout", "30", check=False)
        if claim.returncode == 0:
            break
        if claim.returncode != CLAIM_PENDING or time.monotonic() > deadline:
            raise SystemExit(f"claim failed: {claim.stdout.strip()} {claim.stderr.strip()}")

    for edit in agent["edits"]:
        while ff("agent", "guard", edit["path"], check=False).returncode == 2:
            counters["guard_blocks"] += 1
            time.sleep(POLL_SECONDS)
        apply_edit(edit)

    ff("agent", "done", "--verification", "passed", "--summary", "scripted edits applied",
       "--timeout", str(int(timeout)))
    print(json.dumps(counters))


if __name__ == "__main__":
    main()
