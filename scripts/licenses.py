#!/usr/bin/env python3
"""Writes the assets and Rust crates xolkit ships, with their licenses.

THIRD_PARTY_LICENSES.md is for people; crates/xolkit/licenses.json holds the same for the app's
Licenses page, one entry per asset or crate. Run it through `mise run licenses`. With `--check`,
fails instead of writing when a file is out of date. Uses only `cargo` and the crate sources Cargo
has already downloaded.
"""

import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUTPUT = ROOT / "THIRD_PARTY_LICENSES.md"
APP_OUTPUT = ROOT / "crates" / "xolkit" / "licenses.json"
LICENSE_PREFIXES = ("LICENSE", "LICENCE", "COPYING", "NOTICE", "UNLICENSE")
# Platforms release builds ship for; add a triple here when a new one ships.
TARGETS = [
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
]

# Files the app embeds that are not Rust crates: (name, license, source, license file).
ASSETS = [
    (
        "Geist and Geist Mono fonts",
        "OFL-1.1",
        "https://github.com/vercel/geist-font",
        "crates/xolkit/fonts/OFL.txt",
    ),
    (
        "Base16 color schemes",
        "MIT",
        "https://github.com/tinted-theming/schemes",
        "crates/xolkit/themes/base16/LICENSE",
    ),
    ("Lucide icons, via gpui-kit-assets", "ISC", "https://lucide.dev", None),
]


def cargo(*args):
    # Cargo writes UTF-8; Python on Windows would otherwise decode it as the ANSI code page.
    return subprocess.run(
        ["cargo", *args], cwd=ROOT, check=True, capture_output=True, encoding="utf-8"
    ).stdout


def shipped_packages(metadata):
    """Packages reachable from the workspace through normal and build dependencies on one platform."""
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    seen = set()
    stack = list(metadata["workspace_members"])
    while stack:
        node = nodes[stack.pop()]
        for dep in node["deps"]:
            kinds = {kind["kind"] for kind in dep["dep_kinds"]}
            if kinds != {"dev"} and dep["pkg"] not in seen:
                seen.add(dep["pkg"])
                stack.append(dep["pkg"])
    members = set(metadata["workspace_members"])
    packages = [p for p in metadata["packages"] if p["id"] in seen and p["id"] not in members]
    return sorted(packages, key=lambda p: (p["name"].lower(), p["version"]))


def license_texts(package):
    root = Path(package["manifest_path"]).parent
    files = {f for f in root.iterdir() if f.is_file() and f.name.upper().startswith(LICENSE_PREFIXES)}
    if package.get("license_file"):
        extra = root / package["license_file"]
        if extra.is_file():
            files.add(extra)
    return [normalize(f.read_text(encoding="utf-8", errors="replace")) for f in sorted(files)]


# "Copyright (c) 2020 …", "Copyright 2020 …", "© 2020 …"; not prose that starts with "copyright".
COPYRIGHT_LINE = re.compile(r"\s*(copyright\s*(\(c\)|©|\d{4})|\(c\)\s*\d{4}|©\s*\d{4})", re.IGNORECASE)


def split_copyright(text):
    """The copyright lines of a license text, and the rest of it."""
    lines = text.split("\n")
    is_copyright = lambda line: COPYRIGHT_LINE.match(line) is not None
    notices = [line.strip() for line in lines if is_copyright(line)]
    body = normalize("\n".join(line for line in lines if not is_copyright(line)))
    # The Apache appendix only explains how to apply the license; variants differ there.
    end = body.find("END OF TERMS AND CONDITIONS")
    if end != -1:
        body = body[: end + len("END OF TERMS AND CONDITIONS")]
    # A leading title line ("MIT License", "The MIT License (MIT)") is shown as the heading.
    rest = body.split("\n")
    while rest and len(rest[0].split()) <= 6 and "license" in rest[0].lower():
        rest = rest[1:]
    return notices, normalize("\n".join(rest))


# Phrases that identify a license text, checked in order. The OFL quotes the MIT grant, so it
# comes first.
KNOWN_LICENSES = [
    ("SIL Open Font License", "SIL Open Font License 1.1"),
    ("Community Data License Agreement - Permissive - Version 2.0", "CDLA Permissive 2.0"),
    ("CC0 1.0 Universal", "Creative Commons CC0 1.0"),
    ("GNU General Public License Version 2", "GNU General Public License 2.0"),
    ("dual-licensed under the Unlicense and MIT", "Unlicense or MIT"),
    ("Apache License", "Apache License 2.0"),
    ("Mozilla Public License", "Mozilla Public License 2.0"),
    ("Boost Software License", "Boost Software License 1.0"),
    ("UNICODE LICENSE", "Unicode License v3"),
    ("Zero Clause", "BSD Zero Clause License"),
    ("free and unencumbered software", "The Unlicense"),
    ("Permission is hereby granted, free of charge", "MIT License"),
    ("Permission to use, copy, modify, and/or distribute", "ISC License"),
    ("provided 'as-is'", "zlib License"),
    ("Neither the name", "BSD 3-Clause License"),
    ("Redistribution and use in source and binary forms", "BSD 2-Clause License"),
]


def license_name(body):
    flat = " ".join(body.split())
    for phrase, name in KNOWN_LICENSES:
        if phrase.lower() in flat.lower():
            return name
    return "Other license and notices"


def normalize(text):
    lines = [line.rstrip() for line in text.replace("\r\n", "\n").split("\n")]
    return "\n".join(lines).strip("\n")


def fence(text):
    # A fence longer than any backtick run inside the text.
    longest = max((len(run) for run in text.split("\n") if set(run) == {"`"}), default=0)
    ticks = "`" * max(3, longest + 1)
    return f"{ticks}text\n{text}\n{ticks}"


def group_texts(things):
    """Entries for the app, one per thing, and the license texts they point to.

    Texts that differ only in their copyright lines are one text, which lists everyone using it and
    all their copyright lines; each entry keeps only its own.
    """
    keys = {}  # whitespace-insensitive body -> index into texts
    texts = []
    entries = []
    for thing in things:
        refs = {}  # index into texts -> this thing's copyright lines
        for text in thing["texts"]:
            notices, body = split_copyright(text)
            key = re.sub(r"https?://", "", " ".join(body.split()))
            if key not in keys:
                keys[key] = len(texts)
                texts.append({"name": license_name(text), "body": body, "users": [], "notices": []})
            ix = keys[key]
            user = f"{thing['name']} {thing['version']}".strip()
            if user not in texts[ix]["users"]:
                texts[ix]["users"].append(user)
            texts[ix]["notices"] += [n for n in notices if n not in texts[ix]["notices"]]
            own = refs.setdefault(ix, [])
            own += [n for n in notices if n not in own]
        entries.append(
            {
                "name": thing["name"],
                "version": thing["version"],
                "license": thing["license"],
                "link": thing["link"],
                "texts": [{"text": ix, "notices": notices} for ix, notices in refs.items()],
            }
        )
    return entries, texts


def render():
    """The Markdown file and the app's JSON."""
    cargo("fetch", "--locked")
    by_id = {}
    for target in TARGETS:
        metadata = json.loads(
            cargo("metadata", "--format-version", "1", "--locked", "--filter-platform", target)
        )
        by_id.update({p["id"]: p for p in shipped_packages(metadata)})
    packages = sorted(by_id.values(), key=lambda p: (p["name"].lower(), p["version"]))

    things = [
        {
            "name": name,
            "version": "",
            "license": spdx,
            "link": url,
            "texts": [normalize((ROOT / path).read_text(encoding="utf-8"))] if path else [],
        }
        for name, spdx, url, path in ASSETS
    ]
    for package in packages:
        name = package["name"]
        things.append(
            {
                "name": name,
                "version": package["version"],
                "license": package.get("license") or "see repository",
                "link": package.get("repository") or f"https://crates.io/crates/{name}",
                "texts": license_texts(package),
            }
        )
    entries, texts = group_texts(things)

    out = [
        "# Third-party licenses",
        "",
        "xolkit is released under the [MIT License](LICENSE). It is built on the work below;",
        "thank you to everyone who made it.",
        "",
        "This file is generated by `mise run licenses`; do not edit it by hand.",
        "",
        "## Bundled assets",
        "",
        "| Asset | License | Source |",
        "| --- | --- | --- |",
    ]
    for name, spdx, url, _ in ASSETS:
        out.append(f"| {name} | {spdx} | <{url}> |")

    out += [
        "",
        "## Rust crates",
        "",
        f"{len(packages)} crates, built into the {', '.join(TARGETS[:-1])} and {TARGETS[-1]} releases.",
        "",
    ]
    out += ["| Crate | Version | License |", "| --- | --- | --- |"]
    crates = entries[len(ASSETS) :]
    for entry in crates:
        out.append(f"| [{entry['name']}]({entry['link']}) | {entry['version']} | {entry['license']} |")

    out += ["", "## License texts", ""]
    for text in texts:
        out += [f"### {text['name']}", "", "Used by " + ", ".join(text["users"]) + ".", ""]
        if text["notices"]:
            out += ["Copyright notices:", ""] + [f"- {n}" for n in text["notices"]] + [""]
        out += [fence(text["body"]), ""]
    missing = [e for e in crates if not e["texts"]]
    if missing:
        out += [
            "### Crates without a license file",
            "",
            "These crates publish no license file; their license is the one listed above, and the",
            "full text is in their repository.",
            "",
        ]
        out += [f"- {e['name']} {e['version']} ({e['license']})" for e in missing]
        out.append("")
    markdown = "\n".join(out).rstrip("\n") + "\n"

    app = {
        "entries": entries,
        "texts": [{"name": t["name"], "body": t["body"]} for t in texts],
    }
    return markdown, json.dumps(app, indent=1, ensure_ascii=False) + "\n"


def selftest():
    notices, body = split_copyright(
        "MIT License\n\nCopyright (c) 2020 A\n© 2021 B\nThe above copyright notice shall be included."
    )
    assert notices == ["Copyright (c) 2020 A", "© 2021 B"], notices
    assert body == "The above copyright notice shall be included.", body
    assert license_name("Permission is hereby granted, free of charge, to any") == "MIT License"
    assert license_name("the SIL Open Font License. Permission is hereby granted, free of charge") == (
        "SIL Open Font License 1.1"
    )
    assert fence("a\n```\nb").startswith("````text")

    mit = "Permission is hereby granted, free of charge, to any person."
    thing = lambda name, *texts: {"name": name, "version": "1.0", "license": "MIT", "link": "", "texts": list(texts)}
    entries, texts = group_texts(
        [thing("a", f"Copyright 2020 A\n{mit}"), thing("b", f"Copyright 2021 B\n{mit}", "Other terms.")]
    )
    assert [t["users"] for t in texts] == [["a 1.0", "b 1.0"], ["b 1.0"]], texts
    assert texts[0]["notices"] == ["Copyright 2020 A", "Copyright 2021 B"], texts
    assert entries[1]["texts"] == [
        {"text": 0, "notices": ["Copyright 2021 B"]},
        {"text": 1, "notices": []},
    ], entries


def main():
    selftest()
    outputs = dict(zip((OUTPUT, APP_OUTPUT), render()))
    if "--check" in sys.argv[1:]:
        # Reading in text mode also accepts a checkout with CRLF line endings.
        stale = [p for p, text in outputs.items() if not p.exists() or p.read_text(encoding="utf-8") != text]
        if stale:
            names = " and ".join(p.relative_to(ROOT).as_posix() for p in stale)
            sys.exit(f"{names} out of date; run `mise run licenses`.")
        return
    for path, text in outputs.items():
        path.write_text(text, encoding="utf-8", newline="\n")


if __name__ == "__main__":
    main()
