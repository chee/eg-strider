"""Draws the comparison charts from the results of `compare --json`.

    python3 compare/figures.py compare/figures/results.json compare/figures
"""

import json
import math
import sys

COLOURS = {"S": "#1b9e77", "C": "#d95f02", "A": "#7570b3"}
ORDER = ["S1", "S2", "S3", "C1", "C2", "A1", "A2"]
LEFT, RIGHT, ROW, GAP = 200, 640, 12, 16


def milliseconds(v):
    if v >= 1000:
        return f"{v / 1000:.1f} sec"
    return f"{v:.2f} ms" if v < 10 else f"{v:.1f} ms" if v < 100 else f"{v:.0f} ms"


def memory(v):
    for unit, size in (("GiB", 2**30), ("MiB", 2**20), ("KiB", 2**10)):
        if v >= size:
            return f"{v / size:.1f} {unit}" if v < 10 * size else f"{v / size:.0f} {unit}"
    return f"{v:.0f} B"


def chart(results, kind, groups, label, caption, ticks):
    by_trace = {r["trace"]: r[kind] for r in results}
    traces = [t for t in ORDER if t in by_trace]
    values = [by_trace[t][key] for _, _, key in groups for t in traces]
    values = [v for v in values if v]
    low, high = math.floor(math.log10(min(values))), math.ceil(math.log10(max(values)))
    x = lambda v: LEFT + (math.log10(v) - low) / (high - low) * (RIGHT - LEFT)

    height = len(groups) * (len(traces) * ROW + GAP) + 50
    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {RIGHT + 110} {height}" font-family="Helvetica, Arial, sans-serif">',
        f'<rect width="{RIGHT + 110}" height="{height}" fill="white"/>',
    ]
    bottom = height - 40
    for decade in range(low, high):
        for step in range(1, 10):
            at = x(step * 10**decade)
            out.append(f'<line x1="{at:.1f}" y1="6" x2="{at:.1f}" y2="{bottom}" stroke="#ddd" stroke-width="{1 if step == 1 else 0.5}"/>')
    for value, text in ticks:
        if 10**low <= value <= 10**high:
            out.append(f'<line x1="{x(value):.1f}" y1="{bottom}" x2="{x(value):.1f}" y2="{bottom + 5}" stroke="black"/>')
            out.append(f'<text x="{x(value):.1f}" y="{bottom + 17}" font-size="10" text-anchor="middle">{text}</text>')
    out.append(f'<line x1="{LEFT}" y1="{bottom}" x2="{RIGHT}" y2="{bottom}" stroke="black"/>')
    out.append(f'<text x="{RIGHT}" y="{bottom + 33}" font-size="10" text-anchor="end">{caption} →</text>')

    y = 8
    for name, note, key in groups:
        middle = y + len(traces) * ROW / 2
        out.append(f'<text x="{LEFT - 40}" y="{middle - (4 if note else -4):.1f}" font-size="14" text-anchor="end">{name}</text>')
        if note:
            out.append(f'<text x="{LEFT - 40}" y="{middle + 10:.1f}" font-size="10" fill="#666" text-anchor="end">({note})</text>')
        out.append(f'<line x1="{LEFT}" y1="{y - 2}" x2="{LEFT}" y2="{y + len(traces) * ROW}" stroke="black" stroke-width="1.5"/>')
        for trace in traces:
            value = by_trace[trace][key]
            out.append(f'<text x="{LEFT - 6}" y="{y + 8}" font-size="7.5" fill="#888" text-anchor="end">{trace}</text>')
            if value:
                end = x(value)
                out.append(f'<rect x="{LEFT}" y="{y}" width="{max(end - LEFT, 1):.1f}" height="{ROW - 2}" fill="{COLOURS[trace[0]]}"/>')
                out.append(f'<text x="{end + 5:.1f}" y="{y + 8.5}" font-size="9">{label(value)}</text>')
            else:
                out.append(f'<text x="{LEFT + 5}" y="{y + 8.5}" font-size="9" fill="#888">{"nothing" if value == 0 else "did not finish"}</text>')
            y += ROW
        y += GAP
    out.append("</svg>")
    return "\n".join(out)


def main(results_path, out_dir):
    results = json.load(open(results_path))
    time = chart(
        results,
        "ms",
        [
            ("eg-strider", "", "strider_first"),
            ("Eg-walker", "", "egwalker"),
            ("Loro", "", "loro"),
            ("Yjs", "", "yjs"),
            ("Automerge", "", "automerge"),
        ],
        milliseconds,
        "Time taken to merge all events into a document, from its stored history. Log scale. (Less is better)",
        [(0.1, "0.1 ms"), (1, "1 ms"), (10, "10 ms"), (100, "100 ms"), (1000, "1 sec"), (10000, "10 sec"), (100000, "100 sec")],
    )
    ram = chart(
        results,
        "bytes",
        [
            ("eg-strider", "", "strider_first"),
            ("Eg-walker", "", "egwalker"),
            ("Loro", "", "loro"),
            ("Yjs", "", "yjs"),
            ("Automerge", "", "automerge"),
        ],
        memory,
        "Peak RAM used while merging, beyond the stored history. Log scale. (Less is better)",
        [(2**10 * 100, "100 KiB"), (2**20, "1 MiB"), (2**20 * 10, "10 MiB"), (2**20 * 100, "100 MiB"), (2**30, "1 GiB")],
    )
    open(f"{out_dir}/time.svg", "w").write(time)
    open(f"{out_dir}/memory.svg", "w").write(ram)


if __name__ == "__main__":
    main(*sys.argv[1:3])
