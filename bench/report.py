"""Summarize Gungraun 0.20 Callgrind results without summing derived rates."""

import argparse
import json
from pathlib import Path

# Raw additive events, unlike derived rates, may be summed across threads.
ADDITIVE = ("Ir", "Dr", "Dw", "I1mr", "D1mr", "D1mw", "ILmr", "DLmr", "DLmw",
            "Bc", "Bcm", "Bi", "Bim", "SysCount", "SysTime", "SysCpuTime")


def number(metrics, event, side):
    return metrics.get(event, {}).get("values", {}).get(side)


def rate(numerator, denominator):
    return f"{100 * numerator / denominator:.3f}%" if denominator else "n/a"


def report(path):
    summary = json.loads(path.read_text())
    reported = 0
    for profile in summary["profiles"]:
        if profile["tool"] != "Callgrind":
            continue
        data = profile["data"]
        total = data["total"]["metrics"]
        for side in ("new", "old"):
            if number(total, "Ir", side) is None:
                continue
            for event in ADDITIVE:
                expected = number(total, event, side)
                if expected is None:
                    continue
                actual = sum(number(p["metrics"], event, side) or 0 for p in data["parts"])
                if actual != expected:
                    raise ValueError(f"{path}: {side} {event}: parts sum {actual} != total {expected}")
        side = "new" if number(total, "Ir", "new") is not None else "old"
        counts = {event: number(total, event, side) for event in ADDITIVE}
        instructions = counts["Ir"]
        if not instructions:
            raise ValueError(f"{path}: no instructions collected; check entry point and tool configuration")
        previous = number(total, "Ir", "old") if side == "new" else None
        change = f"{100 * (instructions / previous - 1):+.4f}%" if previous else "n/a"
        i1 = rate(counts["I1mr"], instructions) if counts["I1mr"] is not None else "n/a"
        d1 = (
            rate(counts["D1mr"] + counts["D1mw"], counts["Dr"] + counts["Dw"])
            if all(counts[k] is not None for k in ("D1mr", "D1mw", "Dr", "Dw"))
            else "n/a"
        )
        name = f'{summary["group"]}::{summary["function_name"]}::{summary.get("id") or ""}'
        print(f"**{name}**\n")
        print("| Benchmark / process fragment | Instructions | Change | I1 miss rate | D1 miss rate |")
        print("|---|---:|---:|---:|---:|")
        print(f"| {name} | {instructions:,} | {change} | {i1} | {d1} |")
        for part in data["parts"]:
            value = number(part["metrics"], "Ir", side)
            run = part["tool_run"].get(side)
            if value is not None and run is not None:
                print(
                    f'| ↳ PID {run["pid"]}, thread {run.get("thread", "n/a")}, '
                    f'part {run.get("part", "n/a")} | {value:,} '
                    f'({100 * value / instructions:.2f}%) | | | |'
                )
        print(f"\nProfiles: `{path.parent}`\n")
        reported += 1
    return reported


def main():
    parser = argparse.ArgumentParser(prog="iets-bench-report", description=__doc__)
    parser.add_argument("path", nargs="?", type=Path, default=Path("target/gungraun"))
    args = parser.parse_args()
    paths = [args.path] if args.path.is_file() else sorted(args.path.rglob("summary.json"))
    if not paths:
        parser.error(f"no summary.json files under {args.path}; run with --save-summary=json")
    try:
        reported = sum(report(path) for path in paths)
    except (OSError, ValueError, KeyError, TypeError) as error:
        parser.error(str(error))
    if not reported:
        parser.error("no Callgrind profiles in the supplied summaries")


if __name__ == "__main__":
    main()
