"""Draws the comparison charts from the results of `compare --json`.

    python3 compare/figures.py compare/figures/results.json compare/figures
"""

import json
import math
import sys

# Each system's name, its key in the results, and its colour.
SYSTEMS = [
    ("eg-strider", "strider_first", "#d55e00"),
    ("diamond-types", "egwalker", "#0072b2"),
    ("Yjs (yrs)", "yjs", "#e69f00"),
    ("Loro", "loro", "#009e73"),
    ("Automerge", "automerge", "#cc79a7"),
]
ORDER = ["S1", "S2", "S3", "C1", "C2", "A1", "A2"]
LEFT, RIGHT, ROW, GAP, TOP = 120, 640, 12, 14, 30
TIMES = [(0.1, "0.1 ms"), (1, "1 ms"), (10, "10 ms"), (100, "100 ms"), (1000, "1 sec"), (10000, "10 sec"), (100000, "100 sec")]


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


def chart(results, kind, label, caption, ticks, baseline=None, share=()):
    """Draws one bar per system for each trace. `baseline` names a figure of the trace
    itself to draw above them, with what to call it. The axis counts decades from the
    first tick, and is long enough for the kinds of figure in `share` too, so that charts
    of them line up. A system whose text came out unlike diamond-types' is starred."""
    by_trace = {r["trace"]: r[kind] for r in results}
    same = {r["trace"]: r["same"] for r in results}
    traces = [t for t in ORDER if t in by_trace]
    systems = ([(baseline[1], None, "#999999")] if baseline else []) + SYSTEMS
    base = {r["trace"]: r[baseline[0]] for r in results} if baseline else {}
    value_of = lambda trace, key: base[trace] if key is None else by_trace[trace].get(key)
    origin = ticks[0][0]
    values = [r[k].get(key) for r in results for k in (kind, *share) for _, key, _ in SYSTEMS] + list(base.values())
    decades = [math.log10(v / origin) for v in values + [origin] if v]
    low, high = math.floor(min(decades)), math.ceil(max(decades))
    x = lambda v: LEFT + (math.log10(v / origin) - low) / (high - low) * (RIGHT - LEFT)

    height = TOP + len(traces) * (len(systems) * ROW + GAP) + 40
    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {RIGHT + 110} {height}" font-family="Helvetica, Arial, sans-serif">',
        f'<rect width="{RIGHT + 110}" height="{height}" fill="white"/>',
    ]
    at = LEFT
    for name, _, colour in systems:
        out.append(f'<rect x="{at}" y="6" width="10" height="10" fill="{colour}"/>')
        out.append(f'<text x="{at + 14}" y="15" font-size="11">{name}</text>')
        at += 26 + 6.2 * len(name)
    if any(text is False for texts in same.values() for text in texts.values()):
        out.append(f'<text x="{RIGHT + 100}" y="15" font-size="9" fill="#888" text-anchor="end">* ends with a different text</text>')

    bottom = height - 40
    for decade in range(low, high):
        for step in range(1, 10):
            at = x(step * 10**decade * origin)
            out.append(f'<line x1="{at:.1f}" y1="{TOP - 4}" x2="{at:.1f}" y2="{bottom}" stroke="#ddd" stroke-width="{1 if step == 1 else 0.5}"/>')
    for value, text in ticks:
        if low <= round(math.log10(value / origin)) <= high:
            out.append(f'<line x1="{x(value):.1f}" y1="{bottom}" x2="{x(value):.1f}" y2="{bottom + 5}" stroke="black"/>')
            out.append(f'<text x="{x(value):.1f}" y="{bottom + 17}" font-size="10" text-anchor="middle">{text}</text>')
    out.append(f'<line x1="{LEFT}" y1="{bottom}" x2="{RIGHT}" y2="{bottom}" stroke="black"/>')
    out.append(f'<text x="{RIGHT}" y="{bottom + 33}" font-size="10" text-anchor="end">{caption} →</text>')

    y = TOP
    for trace in traces:
        middle = y + len(systems) * ROW / 2
        out.append(f'<text x="{LEFT - 62}" y="{middle + 4:.1f}" font-size="14" text-anchor="end">{trace}</text>')
        out.append(f'<line x1="{LEFT}" y1="{y - 2}" x2="{LEFT}" y2="{y + len(systems) * ROW}" stroke="black" stroke-width="1.5"/>')
        least = min(v for v in (by_trace[trace].get(key) for _, key, _ in SYSTEMS) if v)
        for name, key, colour in systems:
            value = value_of(trace, key)
            out.append(f'<text x="{LEFT - 6}" y="{y + 8}" font-size="7.5" fill="#888" text-anchor="end">{name}</text>')
            if value:
                end = x(value)
                out.append(f'<rect x="{LEFT}" y="{y}" width="{max(end - LEFT, 1):.1f}" height="{ROW - 2}" fill="{colour}"/>')
                behind = f' <tspan fill="#888">({times(value / least)}×)</tspan>' if key and value > least else ""
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
    runs = f"Median of {results[0]['reps']} runs. Log scale. (Less is better)"
    load = chart(results, "load_ms", milliseconds, f"Time taken to read a history from its file into memory, ready to merge. {runs}", TIMES, share=["merge_ms"])
    merge = chart(results, "merge_ms", milliseconds, f"Time taken to merge a history in memory into a document. {runs}", TIMES, share=["load_ms"])
    sizes = [(2**20 * 10**k, text) for k, text in ((-1, "0.1 MiB"), (0, "1 MiB"), (1, "10 MiB"), (2, "100 MiB"), (3, "1,000 MiB"), (4, "10,000 MiB"))]
    on_disk = ("disk", "Trace file")
    peak = chart(results, "peak", memory, "Most memory allocated while loading and merging a history. Log scale. (Less is better)", sizes, on_disk, ["retained"])
    retained = chart(results, "retained", memory, "Memory still allocated afterwards, ready to merge again. Log scale. (Less is better)", sizes, on_disk, ["peak"])
    open(f"{out_dir}/time-load.svg", "w").write(load)
    open(f"{out_dir}/time-merge.svg", "w").write(merge)
    open(f"{out_dir}/memory-peak.svg", "w").write(peak)
    open(f"{out_dir}/memory-retained.svg", "w").write(retained)


if __name__ == "__main__":
    main(*sys.argv[1:3])
