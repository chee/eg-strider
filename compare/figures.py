"""Draws the comparison charts from the results of `compare --json`.

    python3 compare/figures.py compare/figures/results.json compare/figures
"""

import json
import math
import sys
import textwrap
from xml.sax.saxutils import escape

# Each system's name, its key in the results, and its colour.
SYSTEMS = [
    ("eg-strider", "strider", "#d55e00"),
    ("diamond-types", "dt", "#0072b2"),
    ("Yjs (yrs)", "yjs", "#e69f00"),
    ("Loro", "loro", "#009e73"),
    ("Automerge", "automerge", "#cc79a7"),
    ("Automerge next", "automerge_next", "#882255"),
]
ORDER = ["S1", "S2", "S3", "C1", "C2", "A1", "A2"]
LEFT, RIGHT, ROW, GAP = 120, 640, 12, 14
TIMES = [(0.01, "0.01 ms"), (0.1, "0.1 ms"), (1, "1 ms"), (10, "10 ms"), (100, "100 ms"), (1000, "1 sec"), (10000, "10 sec"), (100000, "100 sec")]


def milliseconds(v):
    if v >= 1000:
        return f"{v / 1000:.1f} sec"
    return f"{v:.2f} ms" if v < 10 else f"{v:.1f} ms" if v < 100 else f"{v:.0f} ms"


def memory(v):
    for unit, size in (("GiB", 2**30), ("MiB", 2**20), ("KiB", 2**10)):
        if v >= size:
            return f"{v / size:.1f} {unit}" if v < 10 * size else f"{v / size:.0f} {unit}"
    return f"{v:.0f} B"


def times(ratio):
    """How many times the fastest a row is, to three figures."""
    return f"{ratio:.2f}" if ratio < 10 else f"{ratio:.1f}" if ratio < 100 else f"{ratio:.0f}"


def chart(results, kind, title, definition, label, caption, ticks):
    """Draws one bar per system for each trace, under `title` and `definition`. The axis runs in decades of the first tick,
    from the one at or below the smallest figure. A system whose text came out unlike
    diamond-types' is starred."""
    by_trace = {r["trace"]: r[kind] for r in results}
    # A library left out of the run has no figures at all, and no rows.
    shown = [system for system in SYSTEMS if any(r[kind].get(system[1]) is not None for r in results)]
    same = {r["trace"]: r["same"] for r in results}
    traces = [t for t in ORDER if t in by_trace]
    systems = shown
    origin = ticks[0][0]
    values = [r[kind].get(key) for r in results for _, key, _ in shown]
    decades = [math.log10(v / origin) for v in values if v]
    low, high = math.floor(min(decades)), math.ceil(max(decades))
    x = lambda v: LEFT + (math.log10(v / origin) - low) / (high - low) * (RIGHT - LEFT)

    lines = textwrap.wrap(definition, 135, break_on_hyphens=False)
    legend = 44 + 13 * len(lines)
    top = legend + 15
    height = top + len(traces) * (len(systems) * ROW + GAP) + 40
    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {RIGHT + 110} {height}" font-family="Helvetica, Arial, sans-serif">',
        f'<rect width="{RIGHT + 110}" height="{height}" fill="white"/>',
        f'<text x="{LEFT}" y="20" font-size="16" font-weight="bold">{title}</text>',
    ]
    for i, line in enumerate(lines):
        out.append(f'<text x="{LEFT}" y="{36 + 13 * i}" font-size="10" fill="#444">{escape(line)}</text>')
    at = LEFT
    for name, _, colour in systems:
        out.append(f'<rect x="{at}" y="{legend - 9}" width="10" height="10" fill="{colour}"/>')
        out.append(f'<text x="{at + 14}" y="{legend}" font-size="11">{name}</text>')
        at += 26 + 6.2 * len(name)
    if any(text is False for texts in same.values() for text in texts.values()):
        out.append(f'<text x="{RIGHT + 100}" y="{legend}" font-size="9" fill="#888" text-anchor="end">* ends with a different text</text>')

    bottom = height - 40
    for decade in range(low, high):
        for step in range(1, 10):
            at = x(step * 10**decade * origin)
            out.append(f'<line x1="{at:.1f}" y1="{top - 4}" x2="{at:.1f}" y2="{bottom}" stroke="#ddd" stroke-width="{1 if step == 1 else 0.5}"/>')
    for value, text in ticks:
        if low <= round(math.log10(value / origin)) <= high:
            out.append(f'<line x1="{x(value):.1f}" y1="{bottom}" x2="{x(value):.1f}" y2="{bottom + 5}" stroke="black"/>')
            out.append(f'<text x="{x(value):.1f}" y="{bottom + 17}" font-size="10" text-anchor="middle">{text}</text>')
    out.append(f'<line x1="{LEFT}" y1="{bottom}" x2="{RIGHT}" y2="{bottom}" stroke="black"/>')
    out.append(f'<text x="{RIGHT}" y="{bottom + 33}" font-size="10" text-anchor="end">{caption} →</text>')

    y = top
    for trace in traces:
        middle = y + len(systems) * ROW / 2
        out.append(f'<text x="{LEFT - 62}" y="{middle + 4:.1f}" font-size="14" text-anchor="end">{trace}</text>')
        out.append(f'<line x1="{LEFT}" y1="{y - 2}" x2="{LEFT}" y2="{y + len(systems) * ROW}" stroke="black" stroke-width="1.5"/>')
        least = min(v for v in (by_trace[trace].get(key) for _, key, _ in shown) if v)
        for name, key, colour in systems:
            value = by_trace[trace].get(key)
            out.append(f'<text x="{LEFT - 6}" y="{y + 8}" font-size="7.5" fill="#888" text-anchor="end">{name}</text>')
            if value:
                end = x(value)
                out.append(f'<rect x="{LEFT}" y="{y}" width="{max(end - LEFT, 1):.1f}" height="{ROW - 2}" fill="{colour}"/>')
                behind = f' <tspan fill="#888">({times(value / least)}×)</tspan>' if value > least else ""
                star = "*" if same[trace].get(key) is False else ""
                out.append(f'<text x="{end + 5:.1f}" y="{y + 8.5}" font-size="9">{label(value)}{star}{behind}</text>')
            else:
                out.append(f'<text x="{LEFT + 5}" y="{y + 8.5}" font-size="9" fill="#888">{"nothing" if value == 0 else "did not finish"}</text>')
            y += ROW
        y += GAP
    out.append("</svg>")
    return "\n".join(out)


def main(results_path, out_dir):
    results = json.load(open(results_path))
    scale = "Log scale. (Less is better)"
    runs = f"Median of {results[0]['reps']} runs. {scale}"
    sizes = [(2**20 * 10**k, text) for k, text in ((-2, "0.01 MiB"), (-1, "0.1 MiB"), (0, "1 MiB"), (1, "10 MiB"), (2, "100 MiB"), (3, "1,000 MiB"), (4, "10,000 MiB"))]
    charts = {
        "time-merge": chart(
            results,
            "merge_ms",
            "Merge time",
            "From the bytes of each library's own encoding of a whole history until the merged text is read, with all decoding counted. "
            "eg-strider and diamond-types replay their event graph from nothing; Yjs, Loro (from its updates) and Automerge integrate their CRDT ops.",
            milliseconds,
            runs,
            TIMES,
        ),
        "time-open": chart(
            results,
            "open_ms",
            "Reopen time",
            "From the bytes of a saved document (each library's native format) until its text is read, with all parsing counted. "
            "eg-strider and Loro leave the history unread until an edit needs it. diamond-types has no saved document that skips the merge, so it is "
            "given what the paper uses: its history file beside a copy of the text. A Yjs or Automerge document is its CRDT, so this is its merge again.",
            milliseconds,
            runs,
            TIMES,
        ),
        "memory-peak": chart(
            results, "merge_peak", "Peak memory", "The most heap in use while merging, the encoded history being merged included.", memory, scale, sizes
        ),
        "memory-at-rest": chart(
            results, "merge_kept", "Ready memory at rest", "What a document holds once a merge from nothing is done, ready for more edits.", memory, scale, sizes
        ),
        "file-size": chart(results, "saved", "Snapshot size", "The size of a saved document, in each library's native format.", memory, scale, sizes),
        "file-size-zstd": chart(
            results,
            "saved_zstd",
            "Compressed snapshot size",
            "The size of a saved document compressed by Zstandard at level 3, which eg-strider already applies to its own.",
            memory,
            scale,
            sizes,
        ),
    }
    for name, drawn in charts.items():
        open(f"{out_dir}/{name}.svg", "w").write(drawn)


if __name__ == "__main__":
    main(*sys.argv[1:3])
