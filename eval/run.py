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
Before editing a file, run `feanorfs agent claim <path>`; it waits until the \
file is yours. Exiting when finished releases it."""

PLAIN_PROMPT = """You are coding agent `{name}`. Task: {task}
Edit the files in the current directory; do not commit."""


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


def agent_command(args, scenario_path, agent, plain=False, settings=None):
    if not args.agent_cmd:
        return [sys.executable, str(HERE / "scripted_agent.py"), str(scenario_path)]
    prompt = (PLAIN_PROMPT if plain else PROMPT).format(
        name=agent["name"], machine=agent["machine"], task=agent["prompt"],
    )
    return shlex.split(args.agent_cmd.format(
        prompt=shlex.quote(prompt), name=agent["name"], machine=agent["machine"],
        settings=shlex.quote(str(settings or HERE / "empty-settings.json")),
    ))


def write_hook_settings(path, binary):
    """Claude Code settings that run the protocol for the agent: a claiming
    guard before every write and `agent done` when it stops."""
    def hook(args):
        return {"type": "command", "command": f"{shlex.quote(str(binary))} {args}", "timeout": 300}
    path.write_text(json.dumps({"hooks": {
        "PreToolUse": [{"matcher": "Edit|Write|MultiEdit|NotebookEdit",
                        "hooks": [hook("agent guard --hook --claim")]}],
        "Stop": [{"hooks": [hook("agent done --hook")]}],
    }}), encoding="utf-8")
    return path


def cost_from(output):
    """Sums `total_cost_usd` from a harness's JSON stdout (Claude Code)."""
    total = 0.0
    for line in output.splitlines():
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict):
            total += float(value.get("total_cost_usd") or 0)
    return round(total, 4)


# Disjoint usage counters only: OpenAI-style `cached_input_tokens` and
# `reasoning_output_tokens` are already inside `input_tokens`/`output_tokens`.
TOKEN_KEYS = ("input_tokens", "output_tokens", "cache_creation_input_tokens",
              "cache_read_input_tokens")


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
            total += sum(usage.get(key, 0) for key in TOKEN_KEYS
                         if isinstance(usage.get(key), int))
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
        # Agents call `feanorfs` by name: resolve it to the binary under test,
        # never an older installed release.
        env.update(FEANORFS_HOME=str(self.home), FEANORFS_EVAL_BIN=str(self.binary),
                   FEANORFS_NO_LAUNCH="1",
                   PATH=f"{self.binary.parent}{os.pathsep}{env.get('PATH', '')}")
        return env

    def run(self, *args, check=True):
        result = subprocess.run([str(self.binary), *args], cwd=self.folder, env=self.env(),
                                capture_output=True, text=True)
        if check and result.returncode != 0:
            raise SystemExit(f"[{self.name}] feanorfs {' '.join(args)}: {result.stderr.strip()}")
        return result

    def json(self, *args):
        return json.loads(self.run("--json", *args).stdout)

    def popen(self, *args, log=None):
        """Starts a long-lived child; output goes to `log` files (not pipes)
        so a grandchild that outlives its parent cannot hang the harness."""
        out = open(f"{log}.out", "w", encoding="utf-8") if log else subprocess.DEVNULL
        err = open(f"{log}.err", "w", encoding="utf-8") if log else subprocess.DEVNULL
        return subprocess.Popen([str(self.binary), *args], cwd=self.folder, env=self.env(),
                                stdout=out, stderr=err)


def stop(process, grace=15):
    """SIGTERM first so `agent run` tears down its child tree, then SIGKILL."""
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=grace)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def coordinate(machine, stats):
    """One pass of the product's automatic coordinator acting for `human`:
    it accepts scope that overlaps no live scope and lets overlap wait."""
    result = machine.run("--json", "agent", "coordinate", check=False)
    if result.returncode == 0:
        stats["coordinator_decisions"] += len(json.loads(result.stdout)["accepted"])


def run_feanorfs(args, scenario, scenario_path, tmp):
    stats = {"mode": "feanorfs", "human_interruptions": 0, "coordinator_decisions": 0,
             "conflicts": 0, "guard_blocks": 0, "tokens": 0, "agents_failed": 0}
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
            settings = write_hook_settings(tmp / "hooks.json", args.bin) if args.hooks else None
            command = agent_command(args, scenario_path, agent, plain=args.hooks, settings=settings)
            log = tmp / f"agent-{agent['name']}"
            processes.append(
                (agent, log, machine.popen("agent", "run", agent["name"], "--", *command, log=log))
            )

        coordinator = machines[names[0]]
        deadline = time.monotonic() + args.timeout
        while any(p.poll() is None for _, _, p in processes) and time.monotonic() < deadline:
            coordinate(coordinator, stats)
            time.sleep(POLL_SECONDS)
        for agent, log, process in processes:
            stop(process)
            out = Path(f"{log}.out").read_text(encoding="utf-8")
            err = Path(f"{log}.err").read_text(encoding="utf-8")
            stats["tokens"] += tokens_from(out)
            stats["cost_usd"] = round(stats.get("cost_usd", 0) + cost_from(out), 4)
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
            stop(watcher)
        for _ in range(2):
            for machine in machines.values():
                machine.run("sync", "--no-watch", check=False)
        stats["conflicts"] = sum(len(m.json("conflicts")) for m in machines.values())
        stats["human_interruptions"] += stats["conflicts"]
        final = machines[names[0]].folder
        stats["lost_edits"] = expectations_missing(final, scenario.get("expect", {}))
        stats["tests_pass"] = verify(final, scenario.get("verify"))
    finally:
        for _, _, process in processes:
            stop(process)
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
            result = subprocess.run(agent_command(args, scenario_path, agent, plain=True), cwd=path,
                                    capture_output=True, text=True, timeout=args.timeout)
            log = tmp / f"agent-{agent['name']}"
            Path(f"{log}.out").write_text(result.stdout, encoding="utf-8")
            Path(f"{log}.err").write_text(result.stderr, encoding="utf-8")
            stats["tokens"] += tokens_from(result.stdout)
            stats["cost_usd"] = round(stats.get("cost_usd", 0) + cost_from(result.stdout), 4)
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
    parser.add_argument("--hooks", action="store_true",
                        help="real agents get claim/done hooks and the bare task prompt")
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
