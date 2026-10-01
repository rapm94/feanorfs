# Multi-agent evaluation

Measures what the coordination layer is for: agents on several machines
finishing shared work with few human interruptions and no lost edits.

`run.py` runs one scenario two ways and prints one JSON line per mode:

| Mode | Setup |
|---|---|
| `feanorfs` | Each machine gets its own `FEANORFS_HOME` and Git clone; one HTTP hub connects them; agents run under `feanorfs agent run`; an auto-coordinator stands in for the human and accepts proposals whose scope does not overlap another agent's live accepted scope. |
| `worktrees` | The same agents work on separate branches in Git worktrees, merged afterwards. Baseline for comparison. |

Metrics:

| Field | Meaning |
|---|---|
| `human_interruptions` | Coordinator decisions plus unresolved conflicts (feanorfs); merge conflicts (worktrees) |
| `conflicts` | Pending FeanorFS conflicts after the run, or failed merges |
| `lost_edits` | Expected strings (`expect`) missing from the final tree |
| `tests_pass` | Scenario `verify` command in the final tree |
| `guard_blocks` | Times an agent's `agent guard` check refused a write |
| `tokens` | Summed from agent JSON output (`tokens`, or `usage.*tokens`) |
| `cost_usd` | Summed `total_cost_usd` from agent JSON output (real agents) |
| `wall_seconds` | End to end, including setup |

## Run

```bash
cargo build --bin feanorfs
python3 eval/run.py eval/scenarios/overlap.json            # both modes, scripted agents
python3 eval/run.py eval/scenarios/disjoint.json --mode feanorfs --keep
```

The exit status is non-zero when any agent fails or a FeanorFS run ends with
conflicts, lost edits, or failing tests. The worktree baseline never fails
the run: its conflicts are the comparison. Main-branch CI runs every scenario
with scripted agents (`agent-eval` job) and uploads the JSON report.

## Real agents

`--agent-cmd` replaces the scripted agent with any harness; `{prompt}`,
`{name}`, and `{machine}` are substituted and shell-quoted. In `feanorfs`
mode the prompt tells the agent its identity, task, and the coordination loop
(`agent next`, `agent work propose`, `agent guard`), and `feanorfs` on the
agent's `PATH` is the binary under test; in `worktrees` mode the prompt is
the bare task. For example, with Claude Code:

```bash
python3 eval/run.py eval/scenarios/overlap.json --timeout 900 \
  --agent-cmd "claude -p {prompt} --output-format json --setting-sources project --strict-mcp-config --permission-mode acceptEdits --allowedTools 'Bash(feanorfs:*)' 'Bash(python3 -m unittest:*)'"
```

`--setting-sources project --strict-mcp-config` keeps your personal hooks,
command rewriters, and MCP servers out of the agents, so runs are comparable.
Keep `--allowedTools` last: it takes several values. Real runs call a model
and cost money; they are not part of CI. With `--keep`, each agent's output
stays in `agent-<name>.out`/`.err` under the kept directory.

## Scenarios

`scenarios/*.json` define the fixture `files`, a `verify` command, `expect`
strings, and per-agent `name`, `machine`, `scope`, `prompt` (real agents),
and `edits` (scripted agents: `write` a file or `insert_after` an exact
anchor line). Add scenarios that reflect real collaboration patterns; keep
each one fast enough for CI.

- `disjoint`: two machines edit different directories.
- `overlap`: two machines insert at the same place in one file. Parallel
  branches conflict and lose an edit; scoped turns keep both.
