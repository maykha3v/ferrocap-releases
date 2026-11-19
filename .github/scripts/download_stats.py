"""Records Ferrocap's GitHub release download counts once a day and draws stats/downloads.svg.

Two series:
  * Downloads       - Ferrocap.exe downloads, all releases (people grabbing the app)
  * First launches  - ffmpeg-win64.zip downloads (the app fetches it once on first launch)

No dependencies beyond the Python standard library.
"""
import csv
import datetime as dt
import json
import os
import urllib.request
from pathlib import Path

REPO = os.environ.get("GITHUB_REPOSITORY", "maykha3v/ferrocap-releases")
ROOT = Path(__file__).resolve().parents[2]
CSV = ROOT / "stats" / "downloads.csv"
SVG = ROOT / "stats" / "downloads.svg"
FIELDS = ["date", "downloads", "first_launches"]


def fetch_counts():
    exe = engine = 0
    page = 1
    while True:
        req = urllib.request.Request(
            f"https://api.github.com/repos/{REPO}/releases?per_page=100&page={page}",
            headers={"Accept": "application/vnd.github+json", "User-Agent": "ferrocap-stats"},
        )
        token = os.environ.get("GITHUB_TOKEN")
        if token:
            req.add_header("Authorization", f"Bearer {token}")
        with urllib.request.urlopen(req, timeout=30) as r:
            releases = json.load(r)
        if not releases:
            break
        for rel in releases:
            for a in rel.get("assets", []):
                name = a["name"].lower()
                if name.endswith(".exe"):
                    exe += a["download_count"]
                elif name.startswith("ffmpeg") and name.endswith(".zip"):
                    engine += a["download_count"]
        page += 1
    return exe, engine


def load():
    if not CSV.exists():
        return []
    with CSV.open(newline="") as f:
        return [
            {"date": r["date"], "downloads": int(r["downloads"]), "first_launches": int(r["first_launches"])}
            for r in csv.DictReader(f)
        ]


def save(rows):
    CSV.parent.mkdir(parents=True, exist_ok=True)
    with CSV.open("w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=FIELDS)
        w.writeheader()
        w.writerows(rows)


def nice_max(v):
    v = max(v, 4)
    for step in (1, 2, 2.5, 5):
        for mag in (1, 10, 100, 1000, 10000, 100000, 1000000):
            top = step * mag * 4
            if top >= v:
                return top, step * mag
    return v, v / 4


def esc(s):
    return str(s).replace("&", "&amp;").replace("<", "&lt;")


def draw(rows):
    W, H = 860, 320
    L, R, T, B = 56, 24, 64, 44
    pw, ph = W - L - R, H - T - B
    top, step = nice_max(max(max(r["downloads"], r["first_launches"]) for r in rows))
    n = len(rows)

    def x(i):
        return L + (pw * i / (n - 1) if n > 1 else pw / 2)

    def y(v):
        return T + ph - ph * v / top

    def path(key):
        return " ".join(f"{'M' if i == 0 else 'L'}{x(i):.1f},{y(r[key]):.1f}" for i, r in enumerate(rows))

    last = rows[-1]
    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" font-family="Segoe UI, Helvetica, Arial, sans-serif">',
        "<style>.t{fill:#8b8f98}.g{stroke:#8b8f98;stroke-opacity:.18}.h{fill:#8b8f98;font-weight:600}</style>",
        f'<text x="{L}" y="26" class="h" font-size="15">Ferrocap downloads</text>',
        f'<text x="{L}" y="46" class="t" font-size="12">Updated {esc(last["date"])} · total {last["downloads"]:,} downloads · {last["first_launches"]:,} first launches</text>',
    ]
    # legend
    lx = W - R - 270
    out += [
        f'<rect x="{lx}" y="18" width="12" height="3" rx="1.5" fill="#ff6a1a"/>',
        f'<text x="{lx + 18}" y="24" class="t" font-size="12">Downloads</text>',
        f'<rect x="{lx + 110}" y="18" width="12" height="3" rx="1.5" fill="#4ea1ff"/>',
        f'<text x="{lx + 128}" y="24" class="t" font-size="12">First launches</text>',
    ]
    v = 0
    while v <= top + 1e-9:
        yy = y(v)
        out.append(f'<line x1="{L}" x2="{W - R}" y1="{yy:.1f}" y2="{yy:.1f}" class="g"/>')
        out.append(f'<text x="{L - 8}" y="{yy + 4:.1f}" class="t" font-size="11" text-anchor="end">{v:g}</text>')
        v += step
    # x labels: first, last and up to 4 in between
    idx = sorted({0, n - 1, *[round((n - 1) * k / 5) for k in range(1, 5)]}) if n > 1 else [0]
    for i in idx:
        d = rows[i]["date"][5:]
        out.append(f'<text x="{x(i):.1f}" y="{H - 18}" class="t" font-size="11" text-anchor="middle">{esc(d)}</text>')
    if n > 1:
        area = path("downloads") + f" L{x(n - 1):.1f},{y(0):.1f} L{x(0):.1f},{y(0):.1f} Z"
        out.append(
            '<defs><linearGradient id="a" x1="0" y1="0" x2="0" y2="1">'
            '<stop offset="0" stop-color="#ff6a1a" stop-opacity=".28"/><stop offset="1" stop-color="#ff6a1a" stop-opacity="0"/>'
            "</linearGradient></defs>"
        )
        out.append(f'<path d="{area}" fill="url(#a)"/>')
    out.append(f'<path d="{path("first_launches")}" fill="none" stroke="#4ea1ff" stroke-width="2" stroke-linejoin="round"/>')
    out.append(f'<path d="{path("downloads")}" fill="none" stroke="#ff6a1a" stroke-width="2.5" stroke-linejoin="round"/>')
    for key, col in (("first_launches", "#4ea1ff"), ("downloads", "#ff6a1a")):
        out.append(f'<circle cx="{x(n - 1):.1f}" cy="{y(last[key]):.1f}" r="4" fill="{col}"/>')
    out.append("</svg>")
    SVG.parent.mkdir(parents=True, exist_ok=True)
    SVG.write_text("\n".join(out) + "\n", encoding="utf8")


def main():
    exe, engine = fetch_counts()
    today = dt.datetime.now(dt.timezone.utc).date().isoformat()
    rows = [r for r in load() if r["date"] != today]
    rows.append({"date": today, "downloads": exe, "first_launches": engine})
    rows.sort(key=lambda r: r["date"])
    save(rows)
    draw(rows)
    print(f"{today}: {exe} downloads, {engine} first launches")


if __name__ == "__main__":
    main()
