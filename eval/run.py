#!/usr/bin/env python3
"""Dadhichi coding-eval harness.

For each task under eval/tasks/, copy its fixture into a fresh temp workspace,
run the agent headlessly against the task prompt, then run the task's verifier
(which re-checks the code objectively) and record pass/fail. Prints a per-task
result and an overall score.

The model provider comes from the environment, exactly as the CLI resolves it:

    # Real model (Anthropic) — needs a funded key:
    ANTHROPIC_API_KEY=sk-... DADHICHI_MODEL=claude-haiku-4-5-20251001 python3 eval/run.py

    # Local model via Ollama:
    OLLAMA_HOST=http://localhost:11434 OLLAMA_MODEL=qwen2.5-coder:1.5b python3 eval/run.py

    # No provider set -> offline mock (echoes; expected score 0 — baseline).

Env knobs:
    DADHICHI_BIN         path to the binary (default: target/release or target/debug)
    DADHICHI_EVAL_TIMEOUT per-task seconds (default 180)
"""
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parent
TASKS_DIR = ROOT / "tasks"
ALLOW_TOML = (ROOT / "allow.toml").read_text()
TIMEOUT = int(os.environ.get("DADHICHI_EVAL_TIMEOUT", "180"))

FIXTURE_SKIP = {"task.txt", "verify.sh"}


def find_binary() -> str:
    # Absolute, because tasks run with cwd set to a temp workspace.
    if env := os.environ.get("DADHICHI_BIN"):
        p = Path(env)
        return str(p if p.is_absolute() else (Path.cwd() / p).resolve())
    for profile in ("release", "debug"):
        candidate = REPO / "target" / profile / "dadhichi"
        if candidate.exists():
            return str(candidate)
    return str(REPO / "target" / "debug" / "dadhichi")


def provider_label() -> str:
    if os.environ.get("ANTHROPIC_API_KEY"):
        return f"anthropic ({os.environ.get('DADHICHI_MODEL', 'default')})"
    if os.environ.get("OPENAI_API_KEY"):
        return f"openai ({os.environ.get('DADHICHI_MODEL', 'default')})"
    if os.environ.get("OLLAMA_HOST"):
        return f"ollama ({os.environ.get('OLLAMA_MODEL', 'default')})"
    return "mock (offline — expect 0; baseline only)"


def run_task(binary: str, task_dir: Path):
    prompt = (task_dir / "task.txt").read_text().strip()
    work = Path(tempfile.mkdtemp(prefix="dadhichi-eval-"))
    try:
        for f in task_dir.iterdir():
            if f.name in FIXTURE_SKIP or not f.is_file():
                continue
            shutil.copy2(f, work / f.name)
        # A git repo + an allow-list config, so the agent may edit files
        # without prompting in headless mode.
        subprocess.run(["git", "init", "-q"], cwd=work, check=False)
        (work / ".dadhichi").mkdir(exist_ok=True)
        (work / ".dadhichi" / "config.toml").write_text(ALLOW_TOML)

        start = time.time()
        note = ""
        try:
            proc = subprocess.run(
                [binary, "-p", prompt, "--output-format", "json"],
                cwd=work,
                capture_output=True,
                text=True,
                timeout=TIMEOUT,
            )
            lines = [ln for ln in proc.stdout.splitlines() if ln.strip()]
            if lines:
                try:
                    payload = json.loads(lines[-1])
                    note = (payload.get("result") or payload.get("error") or "")[:70]
                except json.JSONDecodeError:
                    note = lines[-1][:70]
        except subprocess.TimeoutExpired:
            note = "TIMEOUT"
        duration = time.time() - start

        verify = subprocess.run(
            ["bash", str(task_dir / "verify.sh")],
            cwd=work,
            capture_output=True,
            text=True,
        )
        return verify.returncode == 0, duration, note
    finally:
        shutil.rmtree(work, ignore_errors=True)


def main() -> int:
    binary = find_binary()
    if not Path(binary).exists():
        print(
            f"error: dadhichi binary not found at {binary}\n"
            f"build it first:  cargo build -p dadhichi --bin dadhichi --release",
            file=sys.stderr,
        )
        return 2

    tasks = sorted(p for p in TASKS_DIR.iterdir() if p.is_dir())
    print(f"Dadhichi eval  ·  {len(tasks)} task(s)  ·  provider: {provider_label()}")
    print(f"binary: {binary}\n")

    passed = 0
    for task in tasks:
        ok, dur, note = run_task(binary, task)
        passed += ok
        mark = "PASS" if ok else "FAIL"
        print(f"  [{mark}]  {task.name:<24} {dur:5.1f}s   {note}")

    total = len(tasks)
    pct = (100 * passed // total) if total else 0
    print(f"\nScore: {passed}/{total}  ({pct}%)")
    return 0 if passed == total else 1


if __name__ == "__main__":
    sys.exit(main())
