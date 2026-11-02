#!/usr/bin/env python3
"""Draws the architecture diagram in assets/design, in a dark and a light version.

Writes architecture.svg and architecture-light.svg, then renders each to a PNG at twice its size
with the Geist fonts the app embeds, so it looks the same wherever it's shown. Run it through
`mise run diagram` after changing it. Needs `resvg` on your PATH.
"""

import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEST = ROOT / "assets" / "design"
FONTS = ROOT / "crates" / "xolkit" / "fonts"

# The app's xolkit Dark and Light themes, plus status colors for the three provider writes.
PALETTES = {
    "dark": dict(bg="#161513", card="#1C1A17", sec="#24211D", code="#211F1B", border="#38342E",
                 fg="#ECE6DC", muted="#9C9488", primary="#E9A23B", pfg="#1B1407",
                 daemon="#221E17", green="#86B97A", greenbg="#1E241B", blue="#7AA7D9",
                 bluebg="#1A2029", coral="#E07A5F", coralbg="#2A1C17"),
    "light": dict(bg="#FBF9F5", card="#FFFFFF", sec="#F4F0E8", code="#F2EEE6", border="#DDD6C9",
                  fg="#211D17", muted="#6E665A", primary="#B5690C", pfg="#FFFFFF",
                  daemon="#FBF1E2", green="#3F7F3A", greenbg="#ECF4EA", blue="#2F6DAE",
                  bluebg="#EAF1F9", coral="#B84A2E", coralbg="#FBECE7"),
}

W, H = 900, 856

def svg(p):
    out = []
    a = out.append
    def rect(x, y, w, h, fill, stroke, rx=8, dash=None, sw=1):
        d = f' stroke-dasharray="{dash}"' if dash else ""
        a(f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{rx}" fill="{fill}" stroke="{stroke}" stroke-width="{sw}"{d}/>')
    def text(x, y, s, size=11.5, fill=None, weight=400, mono=False, anchor="start", italic=False):
        fam = "Geist Mono" if mono else "Geist"
        st = ' font-style="italic"' if italic else ""
        s = s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
        a(f'<text x="{x}" y="{y}" font-family="{fam}" font-size="{size}" font-weight="{weight}" fill="{fill or p["fg"]}" text-anchor="{anchor}"{st}>{s}</text>')
    def line(d, color, dash=None, start=False, end=True, key="m"):
        ds = f' stroke-dasharray="{dash}"' if dash else ""
        ms = f' marker-start="url(#{key})"' if start else ""
        me = f' marker-end="url(#{key})"' if end else ""
        a(f'<path d="{d}" fill="none" stroke="{color}" stroke-width="1.6"{ds}{ms}{me}/>')

    a(f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}">')
    a("<defs>")
    for key, color in [("mu", p["muted"]), ("pr", p["primary"]), ("bl", p["blue"]), ("co", p["coral"])]:
        a(f'<marker id="{key}" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M0,0 L8,4 L0,8 z" fill="{color}"/></marker>')
    a("</defs>")
    rect(0, 0, W, H, p["bg"], p["bg"], rx=14)

    # App
    rect(24, 24, 852, 88, p["card"], p["border"])
    text(40, 52, "xolkit app", 14, weight=600)
    text(122, 52, "Every change goes through the daemon. The app never edits files itself.", 11.5, p["muted"])
    pills = ["Skills", "Plugins", "MCP servers", "Discover", "Profiles", "Connections", "Settings"]
    pw, gap, x = (852 - 32 - 6 * 8) / 7, 8, 40
    for name in pills:
        on = name == "Connections"
        rect(round(x, 1), 68, round(pw, 1), 28, p["primary"] if on else p["sec"], p["primary"] if on else p["border"], rx=14)
        text(round(x + pw / 2, 1), 86, name, 12, p["pfg"] if on else p["fg"], 500, anchor="middle")
        x += pw + gap

    line("M450 114 V150", p["primary"], start=True, key="pr")
    text(460, 136, "state and events up · commands down", 11, p["muted"], italic=True)

    # Sessions
    rect(24, 152, 200, 356, "none", p["border"], dash="5 4")
    text(38, 176, "Agent sessions", 13, weight=600)
    for i, (title, sub) in enumerate([("Claude Code", "one shim per session"), ("Codex CLI", "one shim per session"), ("Other MCP clients", 'use the "Other" switches')]):
        y = 190 + i * 104
        rect(36, y, 176, 92, p["card"], p["border"], rx=6)
        text(48, y + 22, title, 13, weight=600)
        text(48, y + 40, sub, 11.5, p["muted"])
        rect(48, y + 54, 152, 26, p["code"], p["border"], rx=4)
        text(124, y + 71.5, "xolkit mcp (shim)", 11.5, mono=True, anchor="middle")
        line(f"M200 {y + 67} H266", p["primary"], key="pr")

    # Daemon
    rect(268, 152, 364, 356, p["daemon"], p["primary"], sw=1.6)
    text(282, 176, "xolkit daemon", 14, weight=600)
    text(282, 194, "runs in the background · exits when idle", 11.5, p["muted"])
    comps = [("Session registry", "knows which agent is which", "filters what each one sees"),
             ("Skill server", "catalogue and prompts", "find_skill · load_skill"),
             ("Profile engine", "applies the active profile", "tells sessions what changed"),
             ("MCP gateway", "passes on server tools", "named server__tool"),
             ("Process supervisor", "shims and child servers", "health, restart, stop"),
             ("Event log", "calls, errors, timings", "feeds Connections")]
    for i, (t, s1, s2) in enumerate(comps):
        x = 282 + (i % 2) * 172
        y = 206 + (i // 2) * 72
        rect(x, y, 164, 64, p["sec"], p["border"], rx=6)
        text(x + 10, y + 21, t, 12.5, weight=600)
        text(x + 10, y + 38, s1, 11, p["muted"])
        text(x + 10, y + 54, s2, 11, p["muted"])
    rect(282, 422, 336, 72, p["sec"], p["coral"], rx=6)
    text(292, 443, "Native plugin bridge", 12.5, weight=600)
    text(292, 461, "runs claude / codex plugin install, enable, disable", 11, p["muted"])
    text(292, 477, "for plugins with hooks, subagents, styles or a status line", 11, p["muted"])

    # Outbound
    rect(676, 152, 200, 356, "none", p["border"], dash="5 4")
    text(690, 176, "Outbound MCP servers", 13, weight=600)
    for i, (title, s1, s2) in enumerate([("Local servers", "started by the daemon", "over stdio"),
                                         ("Remote servers", "over HTTP", "one connection, shared"),
                                         ("OS keyring", "MCP sign-ins", "GitHub token")]):
        y = 190 + i * 104
        rect(688, y, 176, 92, p["card"], p["border"], rx=6)
        text(700, y + 22, title, 13, weight=600)
        text(700, y + 40, s1, 11.5, p["muted"])
        text(700, y + 56, s2, 11.5, p["muted"])
    line("M618 304 L686 244", p["muted"], key="mu")
    line("M618 324 L686 336", p["blue"], dash="2 3", key="bl")

    # Daemon <-> store, bridge -> providers
    line("M380 510 V548", p["muted"], start=True, key="mu")
    text(390, 533, "reads, watches, writes", 11, p["muted"], italic=True)
    line("M604 494 V548", p["coral"], dash="6 4", key="co")
    text(614, 533, "the agent's own CLI", 11, p["muted"], italic=True)

    # Store
    rect(24, 550, 516, 240, p["card"], p["border"])
    text(38, 576, "~/.xolkit", 14, weight=600, mono=True)
    text(118, 576, "everything xolkit keeps, in one folder", 11.5, p["muted"])
    chips = [("skills/", "SKILL.md and its files"), ("plugins/", "one folder per plugin"), ("marketplace/", "for native installs"),
             ("settings.json", "agents, profiles, theme"), ("lock.json", "sources, pinned commits"), ("servers.toml", "outbound MCP servers"),
             ("run/", "socket, pids, logs"), ("staging/", "downloads under review"), ("themes/", "extra Base16 themes")]
    for i, (name, desc) in enumerate(chips):
        x = 36 + (i % 3) * 167
        y = 590 + (i // 3) * 66
        rect(x, y, 158, 56, p["code"], p["coral"] if name == "marketplace/" else p["border"], rx=6)
        text(x + 10, y + 22, name, 12, weight=600, mono=True)
        text(x + 10, y + 41, desc, 11, p["muted"])

    # Provider folders
    rect(584, 550, 292, 240, p["card"], p["border"])
    text(598, 576, "~/.claude · ~/.codex", 14, weight=600, mono=True)
    text(598, 594, "xolkit writes only these three things:", 11.5, p["muted"])
    rows = [("① MCP server entry, once", p["greenbg"], p["green"], None),
            ("② router skill, if you turn it on", p["bluebg"], p["blue"], "4 3"),
            ("③ native plugins, via the agent's CLI", p["coralbg"], p["coral"], None)]
    for i, (t, bg, st, dash) in enumerate(rows):
        y = 606 + i * 44
        rect(598, y, 264, 34, bg, st, rx=6, dash=dash)
        text(610, y + 21.5, t, 12, weight=500)
    text(598, 756, "Unregistering an agent in Settings", 11.5, p["muted"])
    text(598, 773, "removes all three.", 11.5, p["muted"])
    line("M528 618 H558 V723 H596", p["coral"], dash="6 4", key="co")

    # Legend
    y = 820
    items = [(p["muted"], None, "mu", "stdio or files"), (p["primary"], None, "pr", "local socket (named pipe on Windows)"),
             (p["blue"], "2 3", "bl", "HTTP"), (p["coral"], "6 4", "co", "native install through the agent's CLI")]
    xs = [24, 168, 452, 552]
    for (c, dash, key, label), x in zip(items, xs):
        line(f"M{x} {y} H{x + 30}", c, dash=dash, key=key)
        text(x + 40, y + 4, label, 11.5, p["muted"])
    a("</svg>")
    return "\n".join(out) + "\n"

def main():
    DEST.mkdir(parents=True, exist_ok=True)
    for name, palette in PALETTES.items():
        stem = "architecture" if name == "dark" else "architecture-light"
        svg_path = DEST / f"{stem}.svg"
        svg_path.write_text(svg(palette))
        subprocess.run(
            ["resvg", "--skip-system-fonts", "--use-fonts-dir", str(FONTS), "--zoom", "2",
             str(svg_path), str(DEST / f"{stem}.png")],
            check=True,
        )


if __name__ == "__main__":
    main()
