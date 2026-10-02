# Multi-agent evaluation

Measures what the coordination layer is for: agents on several machines
finishing shared work with few human interruptions and no lost edits.

`run.py` runs one scenario two ways and prints one JSON line per mode:

| Mode | Setup |
|---|---|
| `feanorfs` | Each machine gets its own `FEANORFS_HOME` and Git clone; one HTTP hub connects them; agents run under `feanorfs agent run`; the product's `agent coordinate` accepts scope for `human` when it overlaps no other agent's live scope. |
| `worktrees` | The same agents work on separate branches in Git worktrees, merged afterwards. Baseline for comparison. |

Metrics:

| Field | Meaning |
|---|---|
| `human_interruptions` | Unresolved conflicts (feanorfs); merge conflicts (worktrees) |
| `coordinator_decisions` | Scope decisions made automatically by `agent coordinate` (feanorfs) |
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
`{name}`, `{machine}`, and `{settings}` are substituted and shell-quoted. In
`feanorfs` mode the prompt tells the agent to `agent claim` before editing
and `agent done` when finished, and `feanorfs` on the agent's `PATH` is the
binary under test. With `--hooks`, agents instead get the bare task and a
settings file (`{settings}`) whose PreToolUse hook claims before every write
and whose Stop hook runs `agent done`, so they spend no turns on
coordination. In `worktrees` mode the prompt is the bare task and
`{settings}` is an empty file. For example, with Claude Code:

```bash
# Hooks: the protocol runs in Claude Code hooks, not in agent turns.
python3 eval/run.py eval/scenarios/overlap.json --timeout 900 --hooks \
  --agent-cmd "claude -p {prompt} --output-format json --settings {settings} --setting-sources project --strict-mcp-config --permission-mode acceptEdits --allowedTools 'Bash(python3 -m unittest:*)'"

# Explicit claim/done calls.
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
