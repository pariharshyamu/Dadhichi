# Dadhichi coding eval

A small, objective harness for measuring whether Dadhichi's agent can actually
*do* coding tasks with a given model — the thing unit tests can't tell you.

Each task under `tasks/` is a fixture with buggy or unimplemented code, a
natural-language `task.txt`, and a `verify.sh` that re-checks the code
objectively (exit 0 = solved). The runner copies each fixture into a fresh temp
workspace, runs the agent **headless** against the prompt, then runs the
verifier and scores pass/fail. Scoring is objective — the verifier runs the
code, it does not trust the model's self-report.

## Run it

Build the binary once, then pick a provider via env (same resolution as the CLI):

```sh
cargo build -p dadhichi --bin dadhichi --release

# Real model (Anthropic) — needs a funded key:
ANTHROPIC_API_KEY=sk-... DADHICHI_MODEL=claude-haiku-4-5-20251001 python3 eval/run.py

# Local model via Ollama:
OLLAMA_HOST=http://localhost:11434 OLLAMA_MODEL=qwen2.5-coder:1.5b python3 eval/run.py

# No provider -> offline mock (echoes; scores 0 — the honest baseline).
python3 eval/run.py
```

Output:

```
Dadhichi eval  ·  3 task(s)  ·  provider: anthropic (claude-haiku-4-5-20251001)
  [PASS]  01-fix-add                3.2s   Fixed add() to return a + b
  [FAIL]  02-implement-reverse      4.1s   ...
  [PASS]  03-fix-fizzbuzz           3.8s   ...

Score: 2/3  (66%)
```

Env knobs: `DADHICHI_BIN` (binary path), `DADHICHI_EVAL_TIMEOUT` (per-task
seconds, default 180).

## Why headless + an allow-list

Headless mode denies prompt-worthy tool calls so nothing blocks on input. A
coding task needs the agent to read and edit files, so the runner drops a
`.dadhichi/config.toml` (`allow = ["Read", "Edit", "Grep"]`) into each
workspace — the permission-rule engine resolves before the deny gate, so those
calls are allowed without prompting. This mirrors a real CI allow-list.

## Add a task

Create `tasks/NN-name/` with:
- `task.txt` — the natural-language instruction given to the agent,
- one or more fixture source files (buggy / stubbed), and
- `verify.sh` — exits 0 iff the task is correctly solved.

Keep fixtures flat (files, not subdirectories) and verifiers dependency-light
(the current tasks use plain `python3`).

## Interpreting the score

- **mock → 0/N** is expected: the offline model just echoes and never emits an
  edit action. A non-zero score means a real model drove the agent's tools to a
  correct edit.
- The tasks are deliberately small (single-file bug fixes / stubs) so even a
  small local coding model has a fair shot; they test the agent's *plumbing*
  (read → reason → write a correct edit), not frontier reasoning.
