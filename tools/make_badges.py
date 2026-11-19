"""Builds assets/badges/<exe>.png: game icons re-centered and scaled so the whole icon fits inside
Discord's round Rich Presence badge (Discord crops small images to a circle, which cuts the corners
off icons drawn on a transparent background, like Roblox's tilted tile).

Only icons with transparent corners get a badge; full-bleed square icons are designed for the crop and
Ferrocap uses Discord's own icon for those.

Usage: python tools/make_badges.py   (needs Pillow; reads KNOWN_GAMES from src/main.rs)
"""
import io, json, math, os, re, sys, urllib.request

from PIL import Image

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "assets", "badges")
SIZE = 256
FILL = 0.96  # farthest visible pixel lands at 96% of the circle's radius


def get(url):
    return urllib.request.urlopen(urllib.request.Request(url, headers={"User-Agent": "Mozilla/5.0"}), timeout=30).read()


def icon_index():
    apps = json.loads(get("https://discord.com/api/v9/applications/detectable"))
    exes, names = {}, {}
    for a in apps:
        if not a.get("icon_hash"):
            continue
        url = f"https://cdn.discordapp.com/app-icons/{a['id']}/{a['icon_hash']}.png?size={SIZE}"
        names.setdefault(a["name"].lower(), url)
        for e in a.get("executables") or []:
            if e.get("os") == "win32" and not e["name"].startswith(">"):
                exes.setdefault(e["name"].replace("\\", "/").split("/")[-1].lower(), url)
    return exes, names


def badge_name(exe):
    """robloxplayerbeta.exe -> robloxplayerbeta.png (Ferrocap builds the same name)."""
    return exe.removesuffix(".exe").replace(" ", "_") + ".png"


def fit_in_circle(im):
    im = im.convert("RGBA")
    a = im.getchannel("A")
    box = a.point(lambda v: 255 if v > 16 else 0).getbbox()
    if not box:
        return None
    art = im.crop(box)
    cx, cy = art.width / 2, art.height / 2
    px = art.getchannel("A").load()
    r = max(
        math.hypot(x + 0.5 - cx, y + 0.5 - cy)
        for y in range(art.height) for x in range(art.width) if px[x, y] > 16
    )
    scale = (SIZE / 2 * FILL) / r
    w, h = max(1, round(art.width * scale)), max(1, round(art.height * scale))
    art = art.resize((w, h), Image.LANCZOS)
    out = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    out.alpha_composite(art, ((SIZE - w) // 2, (SIZE - h) // 2))
    return out


def main():
    src = open(os.path.join(ROOT, "src", "main.rs"), encoding="utf-8").read()
    block = re.search(r"pub const KNOWN_GAMES[^\[]*\[(.*?)\n    \];", src, re.S).group(1)
    games = re.findall(r'\("([^"]+)",\s*"([^"]+)"\)', block)
    exes, names = icon_index()
    os.makedirs(OUT, exist_ok=True)
    made = []
    for exe, name in games:
        url = exes.get(exe) or names.get(name.lower())
        if not url:
            continue
        im = Image.open(io.BytesIO(get(url))).convert("RGBA")
        w, h = im.size
        a = im.getchannel("A")
        if max(a.getpixel(p) for p in [(2, 2), (w - 3, 2), (2, h - 3), (w - 3, h - 3)]) >= 40:
            continue  # full-bleed icon: Discord's circle crop is fine
        badge = fit_in_circle(im)
        if badge:
            badge.save(os.path.join(OUT, badge_name(exe)), optimize=True)
            made.append(exe)
    print(f"{len(made)} badges in {OUT}")
    print("BADGES = " + json.dumps(sorted(made)))


if __name__ == "__main__":
    sys.exit(main())
