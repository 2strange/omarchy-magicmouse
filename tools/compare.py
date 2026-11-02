#!/usr/bin/python3
"""Compare golden traces (daemon-rs/PORT.md, "Replay contract").

usage: tools/compare.py EXPECTED.trace ACTUAL.trace [--events FILE [--config TOML] [--probe]]
           one pair; with --events the input line behind the first divergence is shown
       tools/compare.py --golden DIR --actual DIR
           every case of DIR/manifest.json against ACTUAL/<name>.trace
       tools/compare.py --golden DIR --run BINARY [--keep DIR]
           run `BINARY --replay <events> [--config <toml>] [--probe]` per case (trace on stdout)
           and compare; --keep saves the actual traces there

Lines must match exactly, except numbers in P and B lines: those may differ by up to --tol
(default 1.5e-6: both sides round to 6 decimals, a one-ulp difference can flip the last digit).
--exact compares P and B lines as text. B lines are compared as JSON (key order and number
spelling, 1e-05 vs 1e-5, do not matter). Exit status 0 when everything matches.
"""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from collections import Counter

HERE = os.path.dirname(os.path.abspath(__file__))


def close(a: float, b: float, tol: float) -> bool:
    return a == b or abs(a - b) <= tol


def same_json(a, b, tol: float) -> bool:
    if isinstance(a, bool) or isinstance(b, bool):
        return a is b if isinstance(a, bool) and isinstance(b, bool) else False
    if isinstance(a, (int, float)) and isinstance(b, (int, float)):
        return close(float(a), float(b), tol)
    if isinstance(a, list) and isinstance(b, list):
        return len(a) == len(b) and all(same_json(x, y, tol) for x, y in zip(a, b))
    if isinstance(a, dict) and isinstance(b, dict):
        return a.keys() == b.keys() and all(same_json(a[k], b[k], tol) for k in a)
    return a == b


def same_p(a: str, b: str, tol: float) -> bool:
    pa, pb = a.split(" "), b.split(" ")
    if len(pa) != 4 or len(pb) != 4 or pa[2] != pb[2]:
        return False
    try:
        if not close(float(pa[1]), float(pb[1]), tol):
            return False
        if pa[3] == "-" or pb[3] == "-":
            return pa[3] == pb[3]
        fa, fb = pa[3].split(","), pb[3].split(",")
        if len(fa) != len(fb):
            return False
        for x, y in zip(fa, fb):
            xi, xx, xy = x.split(":")
            yi, yx, yy = y.split(":")
            if xi != yi or not close(float(xx), float(yx), tol) or not close(float(xy), float(yy), tol):
                return False
    except ValueError:
        return False
    return True


def same(a: str, b: str, tol: float | None) -> bool:
    if a == b:
        return True
    if tol is None or a[:2] != b[:2]:
        return False
    if a.startswith("P "):
        return same_p(a, b, tol)
    if a.startswith("B "):
        try:
            return same_json(json.loads(a[2:]), json.loads(b[2:]), tol)
        except ValueError:
            return False
    return False


def read(path: str) -> list[str]:
    with open(path) as f:
        return f.read().splitlines()


def attribution(events: str, config: str | None, probe: bool) -> list[int] | None:
    """Input line number per expected trace line, from a Python rerun (needs python-evdev)."""
    try:
        sys.path.insert(0, HERE)
        import golden
        att: list[int] = []
        golden.replay(read(events), config, probe, att)
        return att
    except (ImportError, SystemExit, OSError) as exc:
        print(f"  (no input attribution: {exc})")
        return None


def diff(name: str, exp: list[str], act: list[str], tol: float | None, context: int,
         events: str | None = None, config: str | None = None, probe: bool = False) -> bool:
    n = min(len(exp), len(act))
    first = next((i for i in range(n) if not same(exp[i], act[i], tol)), None)
    if first is None and len(exp) == len(act):
        print(f"OK    {name}: {len(exp)} lines")
        return True
    if first is None:
        first = n
    ce, ca = Counter(l[:1] for l in exp), Counter(l[:1] for l in act)
    kinds = sorted(set(ce) | set(ca))
    print(f"FAIL  {name}: first divergence at trace line {first + 1}"
          f" (expected {len(exp)} lines, got {len(act)})")
    print("      per kind expected/actual: " + "  ".join(f"{k}={ce[k]}/{ca[k]}" for k in kinds))
    mism = sum(1 for i in range(n) if not same(exp[i], act[i], tol))
    print(f"      {mism} of the first {n} lines differ")
    lo = max(0, first - context)
    for i in range(lo, min(max(len(exp), len(act)), first + context + 1)):
        e = exp[i] if i < len(exp) else "<end>"
        a = act[i] if i < len(act) else "<end>"
        mark = "  " if i < n and same(e, a, tol) else ">>"
        if mark == "  ":
            print(f"   {mark} {i + 1:>7}  {e[:160]}")
        else:
            print(f"   {mark} {i + 1:>7}  expected: {e[:160]}")
            print(f"   {mark} {'':>7}  actual:   {a[:160]}")
    if events:
        att = attribution(events, config, probe)
        if att is not None:
            ev = read(events)
            # the input line that produced the first differing expected line; past the end, the last one
            no = att[first] if first < len(att) else (att[-1] if att else 1)
            print(f"      caused by input line {no} of {events}:")
            for j in range(max(1, no - context), min(len(ev), no + 1) + 1):
                print(f"   {'>>' if j == no else '  '} {j:>7}  {ev[j - 1][:160]}")
    return False


def manifest(golden_dir: str) -> list[dict]:
    with open(os.path.join(golden_dir, "manifest.json")) as f:
        return json.load(f)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("expected", nargs="?")
    ap.add_argument("actual", nargs="?")
    ap.add_argument("--events")
    ap.add_argument("--config")
    ap.add_argument("--probe", action="store_true")
    ap.add_argument("--golden")
    ap.add_argument("--actual-dir", "--actual", dest="actual_dir")
    ap.add_argument("--run")
    ap.add_argument("--keep")
    ap.add_argument("--only", nargs="*")
    ap.add_argument("--context", type=int, default=6)
    ap.add_argument("--tol", type=float, default=1.5e-6)
    ap.add_argument("--exact", action="store_true")
    a = ap.parse_args()
    tol = None if a.exact else a.tol

    if not a.golden:
        if not (a.expected and a.actual):
            ap.error("two traces, or --golden DIR with --actual DIR or --run BINARY")
        ok = diff(a.actual, read(a.expected), read(a.actual), tol, a.context, a.events, a.config, a.probe)
        return 0 if ok else 1

    if not (a.actual_dir or a.run):
        ap.error("--golden needs --actual DIR or --run BINARY")
    results = []
    for case in manifest(a.golden):
        name = case["name"]
        if a.only and name not in a.only:
            continue
        events = os.path.join(a.golden, case["events"])
        config = os.path.join(a.golden, case["config"]) if case.get("config") else None
        exp = read(os.path.join(a.golden, case["trace"]))
        if a.run:
            cmd = [a.run, "--replay", events] + (["--config", config] if config else []) \
                + (["--probe"] if case.get("probe") else [])
            p = subprocess.run(cmd, capture_output=True, text=True)
            if p.returncode != 0:
                print(f"FAIL  {name}: {' '.join(cmd)} exited {p.returncode}\n{p.stderr[-2000:]}")
                results.append((name, False))
                continue
            act = p.stdout.splitlines()
            if a.keep:
                os.makedirs(a.keep, exist_ok=True)
                with open(os.path.join(a.keep, case["trace"]), "w") as f:
                    f.write(p.stdout)
        else:
            path = os.path.join(a.actual_dir, case["trace"])
            if not os.path.exists(path):
                print(f"FAIL  {name}: {path} missing")
                results.append((name, False))
                continue
            act = read(path)
        results.append((name, diff(name, exp, act, tol, a.context, events, config, bool(case.get("probe")))))
    bad = [n for n, ok in results if not ok]
    print(f"\n{len(results) - len(bad)}/{len(results)} cases match" + (": failing " + ", ".join(bad) if bad else ""))
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
