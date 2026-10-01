#!/usr/bin/env python3
"""FeanorFS multi-agent evaluation harness.

Runs one scenario two ways and reports comparable metrics:

* ``feanorfs``: every machine gets its own FEANORFS_HOME and Git clone, one
  HTTP hub connects them, agents run under ``feanorfs agent run``, and an
  auto-coordinator stands in for the human (every decision it makes counts
  as a human interruption).
* ``worktrees``: the same agents work in Git worktrees on separate branches
  that are merged afterwards; every merge conflict counts as an interruption
  and its branch's edits are lost.

Agents are either the deterministic ``scripted_agent.py`` or a real harness
given with ``--agent-cmd`` (``{prompt}``, ``{name}``, and ``{machine}`` are
substituted, shell-quoted). Results are one JSON object per mode.
"""

import argparse
import json
import os
import secrets
import shlex
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
POLL_SECONDS = 0.5

PROMPT = """You are coding agent `{name}` on machine `{machine}` in a FeanorFS \
workspace shared with other agents. Task: {task}
Before editing, propose your scope with `feanorfs agent work propose --task \
{task_id} --coordinator human --path <path>...` and wait until `feanorfs agent \
next` shows it accepted. Check paths with `feanorfs agent guard <path>` before \
writing. When done and verified, follow the settle and complete actions \
`feanorfs agent next` lists for you."""


def git(cwd, *args):
    subprocess.run(
        ["git", "-c", "user.name=eval", "-c", "user.email=eval@example.invalid", *args],
        cwd=cwd, check=True, capture_output=True, text=True,
    )


def write_fixture(root, files):
    for rel, text in files.items():
        path = root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_port(port, timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        with socket.socket() as sock:
            if sock.connect_ex(("127.0.0.1", port)) == 0:
                return
        time.sleep(0.2)
    raise SystemExit(f"hub did not listen on {port}")


def expectations_missing(root, expect):
    missing = 0
    for rel, needles in expect.items():
        path = root / rel
        text = path.read_text(encoding="utf-8") if path.exists() else ""
        missing += sum(needle not in text for needle in needles)
    return missing


def verify(root, command):
    if not command:
        return None
    return subprocess.run(command, shell=True, cwd=root, capture_output=True).returncode == 0


def agent_command(args, scenario_path, agent):
    if not args.agent_cmd:
        return [sys.executable, str(HERE / "scripted_agent.py"), str(scenario_path)]
    prompt = PROMPT.format(
        name=agent["name"], machine=agent["machine"], task=agent["prompt"],
        task_id=agent.get("task", f"{agent['name']}-task"),
    )
    return shlex.split(args.agent_cmd.format(
        prompt=shlex.quote(prompt), name=agent["name"], machine=agent["machine"],
    ))


def tokens_from(output):
    """Sums token counts from a harness's JSON stdout, when it reports them."""
    total = 0
    for line in output.splitlines():
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict):
            total += value.get("tokens", 0)
            usage = value.get("usage") or {}
            total += sum(v for k, v in usage.items() if k.endswith("tokens") and isinstance(v, int))
    return total


class Machine:
    def __init__(self, binary, tmp, name):
        self.binary = binary
        self.name = name
        self.home = tmp / f"home-{name}"
        self.folder = tmp / f"machine-{name}"
        self.home.mkdir()

    def env(self):
        env = dict(os.environ)
        for key in ("FEANORFS_AGENT", "FEANORFS_AGENT_DIR", "FEANORFS_WORKSPACE_ROOT"):
            env.pop(key, None)
        env.update(FEANORFS_HOME=str(self.home), FEANORFS_EVAL_BIN=str(self.binary),
                   FEANORFS_NO_LAUNCH="1")
        return env

    def run(self, *args, check=True):
        result = subprocess.run([str(self.binary), *args], cwd=self.folder, env=self.env(),
                                capture_output=True, text=True)
        if check and result.returncode != 0:
            raise SystemExit(f"[{self.name}] feanorfs {' '.join(args)}: {result.stderr.strip()}")
        return result

    def json(self, *args):
        return json.loads(self.run("--json", *args).stdout)

    def popen(self, *args, **kwargs):
        return subprocess.Popen([str(self.binary), *args], cwd=self.folder, env=self.env(),
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                                **kwargs)


def overlaps(a, b):
    def covers(glob, path):
        return glob == path or (glob.endswith("/**") and path.startswith(glob[:-2]))
    return any(covers(x, y) or covers(y, x) for x in a for y in b)


def coordinate(machine, stats):
    """One auto-coordinator pass: accept proposals whose scope does not
    overlap another agent's live accepted scope; defer the rest."""
    work = machine.json("agent", "work", "status")
    proposals = [p for task in work["tasks"] for p in task["proposals"]]
    live = [p for p in proposals if p["state"] in ("accepted", "settled")]
    for proposal in proposals:
        if proposal["state"] != "proposed":
            continue
        paths = proposal["accepted_scope"]["paths"]
        if any(other["agent"] != proposal["agent"] and overlaps(paths, other["accepted_scope"]["paths"])
               for other in live):
            continue
        machine.run("agent", "work", "decide", proposal["intent_message_id"], "--kind", "accept")
        stats["human_interruptions"] += 1
        live.append(proposal)


def run_feanorfs(args, scenario, scenario_path, tmp):
    stats = {"mode": "feanorfs", "human_interruptions": 0, "conflicts": 0,
             "guard_blocks": 0, "tokens": 0, "agents_failed": 0}
    started = time.monotonic()
    origin = tmp / "origin"
    write_fixture(origin, scenario["files"])
    git(origin, "init", "-q", "-b", "main")
    git(origin, "add", "-A")
    git(origin, "commit", "-q", "-m", "fixture")

    port, token = free_port(), secrets.token_hex(32)
    hub_home = tmp / "hub-home"
    hub_home.mkdir()
    hub = subprocess.Popen(
        [str(args.bin), "serve", "--allow-http", "--port", str(port),
         "--data-dir", str(tmp / "hub-data"), "--token", token],
        env={**os.environ, "FEANORFS_HOME": str(hub_home)},
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    processes = []
    try:
        wait_port(port)
        url, key = f"http://127.0.0.1:{port}", secrets.token_hex(32)
        names = sorted({agent["machine"] for agent in scenario["agents"]})
        machines = {name: Machine(args.bin, tmp, name) for name in names}
        for index, machine in enumerate(machines.values()):
            git(tmp, "clone", "-q", str(origin), str(machine.folder))
            if index == 0:
                machine.run("setup", "--workspace", "eval", "--encryption-key", key,
                            "--token", token, url)
            else:
                machine.run("attach", "eval", "--encryption-key", key,
                            "--server-url", url, "--token", token)
            machine.run("sync", "--no-watch")
        for agent in scenario["agents"]:
            machines[agent["machine"]].run("agent", "spawn", agent["name"])
        watchers = [m.popen("sync") for m in machines.values()]
        for agent in scenario["agents"]:
            machine = machines[agent["machine"]]
            command = agent_command(args, scenario_path, agent)
            processes.append((agent, machine.popen("agent", "run", agent["name"], "--", *command)))

        coordinator = machines[names[0]]
        deadline = time.monotonic() + args.timeout
        while any(p.poll() is None for _, p in processes) and time.monotonic() < deadline:
            coordinate(coordinator, stats)
            time.sleep(POLL_SECONDS)
        for agent, process in processes:
            if process.poll() is None:
                process.kill()
            out, err = process.communicate()
            stats["tokens"] += tokens_from(out)
            for line in out.splitlines():
                if line.startswith("{") and '"guard_blocks"' in line:
                    stats["guard_blocks"] += json.loads(line)["guard_blocks"]
            if process.returncode != 0:
                stats["agents_failed"] += 1
                view = machines[agent["machine"]].run(
                    "--json", "agent", "next", "--for", agent["name"], check=False)
                print(f"[{agent['name']}] exit {process.returncode}: {err.strip()[-400:]}\n"
                      f"  next: {view.stdout.strip()[:1500]} {view.stderr.strip()[:300]}",
                      file=sys.stderr)
        for watcher in watchers:
            watcher.terminate()
            watcher.wait()
        for _ in range(2):
            for machine in machines.values():
                machine.run("sync", "--no-watch", check=False)
        stats["conflicts"] = sum(len(m.json("conflicts")) for m in machines.values())
        stats["human_interruptions"] += stats["conflicts"]
        final = machines[names[0]].folder
        stats["lost_edits"] = expectations_missing(final, scenario.get("expect", {}))
        stats["tests_pass"] = verify(final, scenario.get("verify"))
    finally:
        for _, process in processes:
            if process.poll() is None:
                process.kill()
        hub.terminate()
        hub.wait()
    stats["wall_seconds"] = round(time.monotonic() - started, 1)
    return stats


def run_worktrees(args, scenario, scenario_path, tmp):
    stats = {"mode": "worktrees", "human_interruptions": 0, "conflicts": 0,
             "tokens": 0, "agents_failed": 0}
    started = time.monotonic()
    repo = tmp / "repo"
    write_fixture(repo, scenario["files"])
    git(repo, "init", "-q", "-b", "main")
    git(repo, "add", "-A")
    git(repo, "commit", "-q", "-m", "fixture")
    worktrees = {}
    for agent in scenario["agents"]:
        path = tmp / f"wt-{agent['name']}"
        git(repo, "worktree", "add", "-q", "-b", agent["name"], str(path))
        worktrees[agent["name"]] = path
    sys.path.insert(0, str(HERE))
    from scripted_agent import apply_edit  # noqa: PLC0415

    for agent in scenario["agents"]:
        path = worktrees[agent["name"]]
        if args.agent_cmd:
            result = subprocess.run(agent_command(args, scenario_path, agent), cwd=path,
                                    capture_output=True, text=True, timeout=args.timeout)
            stats["tokens"] += tokens_from(result.stdout)
            stats["agents_failed"] += result.returncode != 0
        else:
            cwd = os.getcwd()
            os.chdir(path)
            try:
                for edit in agent["edits"]:
                    apply_edit(edit)
            finally:
                os.chdir(cwd)
        git(path, "add", "-A")
        git(path, "commit", "-q", "--allow-empty", "-m", agent["name"])
    for agent in scenario["agents"]:
        merged = subprocess.run(["git", "-c", "user.name=eval", "-c",
                                 "user.email=eval@example.invalid", "merge", "-q",
                                 "--no-edit", agent["name"]],
                                cwd=repo, capture_output=True)
        if merged.returncode != 0:
            stats["conflicts"] += 1
            stats["human_interruptions"] += 1
            git(repo, "merge", "--abort")
    stats["lost_edits"] = expectations_missing(repo, scenario.get("expect", {}))
    stats["tests_pass"] = verify(repo, scenario.get("verify"))
    stats["wall_seconds"] = round(time.monotonic() - started, 1)
    return stats


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("scenario", type=Path)
    parser.add_argument("--bin", type=Path, default=HERE.parent / "target/debug/feanorfs")
    parser.add_argument("--mode", choices=["feanorfs", "worktrees", "both"], default="both")
    parser.add_argument("--agent-cmd", help="real agent command template; default: scripted")
    parser.add_argument("--timeout", type=float, default=300)
    parser.add_argument("--keep", action="store_true", help="keep the temporary directory")
    args = parser.parse_args()
    args.bin = args.bin.resolve()
    scenario = json.loads(args.scenario.read_text(encoding="utf-8"))
    modes = ["feanorfs", "worktrees"] if args.mode == "both" else [args.mode]
    results = []
    for mode in modes:
        tmp = Path(tempfile.mkdtemp(prefix=f"feanorfs-eval-{mode}-")).resolve()
        try:
            runner = run_feanorfs if mode == "feanorfs" else run_worktrees
            result = runner(args, scenario, args.scenario.resolve(), tmp)
            result["scenario"] = scenario["name"]
            results.append(result)
            print(json.dumps(result))
        finally:
            if args.keep:
                print(f"kept {tmp}", file=sys.stderr)
            else:
                shutil.rmtree(tmp, ignore_errors=True)
    # The worktree baseline is a comparison, not a gate: its conflicts and
    # lost edits are the measurement. FeanorFS runs must be clean.
    clean = all(
        r["agents_failed"] == 0
        and (r["mode"] != "feanorfs"
             or (r["lost_edits"] == 0 and r["conflicts"] == 0 and r["tests_pass"] is not False))
        for r in results
    )
    return 0 if clean else 1


if __name__ == "__main__":
    sys.exit(main())
