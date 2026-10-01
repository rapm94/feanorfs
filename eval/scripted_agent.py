#!/usr/bin/env python3
"""Deterministic stand-in for a coding agent, run under `feanorfs agent run`.

It follows the same loop a real agent is taught by the skill: propose scope,
wait for an observed decision, check each path with `agent guard`, edit,
wait for the live controller to settle the edit, then settle and complete.
It prints one JSON line with its own counters on exit.
"""

import json
import os
import subprocess
import sys
import time

POLL_SECONDS = 0.5


def ff(*args, check=True):
    result = subprocess.run(
        [os.environ["FEANORFS_EVAL_BIN"], *args],
        capture_output=True,
        text=True,
    )
    if check and result.returncode != 0:
        raise SystemExit(f"feanorfs {' '.join(args)} failed: {result.stderr.strip()}")
    return result


def ff_json(*args):
    return json.loads(ff("--json", *args).stdout)


def wait_for(description, predicate, timeout):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(POLL_SECONDS)
    raise SystemExit(f"timed out waiting for {description}")


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
    task = agent.get("task", f"{name}-task")
    counters = {"agent": name, "guard_blocks": 0, "tokens": 0}

    scope = [arg for path in agent["scope"] for arg in ("--path", path)]
    intent = ff_json(
        "agent", "work", "propose", "--task", task, "--coordinator", "human", *scope
    )["message_id"]

    def my_item():
        status = ff_json("agent", "next")
        return next(
            (item for item in status["items"] if item["id"] == f"{task}:{name}"), None
        )

    item = wait_for(
        "a coordinator decision",
        lambda: (lambda i: i if i and i["stage"] != "proposed" else None)(my_item()),
        timeout,
    )
    if item["stage"] not in ("accepted", "settled", "done"):
        raise SystemExit(f"proposal ended as {item['stage']}")

    def check():
        return ff_json("agent", "status", name)

    # Start from the current shared files, not a stale copy.
    wait_for(
        "remote changes to reach the worktree",
        lambda: not check()["their_changes"],
        timeout,
    )
    for edit in agent["edits"]:
        while ff("agent", "guard", edit["path"], check=False).returncode == 2:
            counters["guard_blocks"] += 1
            time.sleep(POLL_SECONDS)
        apply_edit(edit)

    def settled():
        status = check()
        live = status.get("live") or {}
        done = not status["our_changes"] and not live.get("pending_local", True)
        return done and live.get("settled_snapshot")

    snapshot = wait_for("the live controller to land the edits", settled, timeout)
    ff(
        "agent", "work", "settle", "--task", task, "--intent", intent,
        "--sequence", "2", "--inspected", snapshot,
        "--verification", "passed", "--summary", "scripted edits applied",
    )
    ff(
        "agent", "work", "complete", "--task", task, "--intent", intent,
        "--sequence", "3", "--outcome", "scripted edits landed",
    )
    print(json.dumps(counters))


if __name__ == "__main__":
    main()
