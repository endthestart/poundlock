#!/usr/bin/env python3
"""Build the Poundlock theme from the Catppuccin palette.

One source of truth for every surface — the controller web UI, Dispatch's
GUI and TUI, and the talk deck. `docs/design/catppuccin-palette.json` is the
upstream palette, verbatim; this script maps it to semantic roles and writes:

- `docs/design/theme.json` — every role resolved to a hex per flavour, for
  code that cannot read CSS (the Dispatch TUI, tests).
- `web/theme.css` — the same roles as custom properties under
  `:root[data-theme=<flavour>]`. Dispatch vendors this file unchanged.

Roles name what a colour means, never which hue it is, so a view reads
`--state-fault`, not `--red`. Catppuccin's light flavour (Latte) is too pale
for coloured text on its own backgrounds, so a text or indicator role that
misses its contrast target is mixed toward that flavour's `text` colour until
it passes — derived, never invented, and recorded in theme.json.

    python3 scripts/theme.py          # regenerate
    python3 scripts/theme.py --check  # fail if generated files are stale or a rule is broken
"""

import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
PALETTE = ROOT / "docs/design/catppuccin-palette.json"
THEME_JSON = ROOT / "docs/design/theme.json"
THEME_CSS = ROOT / "web/theme.css"

FLAVOURS = ("latte", "frappe", "macchiato", "mocha")
DARK = {"frappe", "macchiato", "mocha"}

# Surfaces: a four-step ramp, darkest first in the dark flavours. A panel is a
# gradient between two adjacent steps; a nested element steps up exactly one.
SURFACES = {
    "canvas": "crust",
    "s1": "mantle",
    "s2": "base",
    "s3": "surface0",
    "s4": "surface1",
    "line": "surface1",
    "line-strong": "surface2",
}

# Text on the panel surface (s2).
TEXT = {"text": ("text", 4.5), "muted": ("subtext1", 4.5), "faint": ("subtext0", 4.5)}

# Hues with meaning. Each produces `--<role>` (an indicator: dots, ticks,
# borders — 3:1 against s2) and `--<role>-text` (4.5:1 against s2).
MEANINGS = {
    "accent": "teal",
    "accent-2": "mauve",
    "state-ok": "green",
    "state-active": "sapphire",
    "state-review": "yellow",
    "state-fault": "red",
    # Not measured / unknown and deliberately ended are grey, told apart from
    # each other by the dashed treatment in CSS, and from a fault by hue.
    "state-unknown": "overlay1",
    "state-ended": "overlay0",
    # Reserved: a value the view does not recognise. Used nowhere else.
    "state-unrecognised": "pink",
}

# Ownership chips: a separate palette that can never be mistaken for a state.
OWNERSHIP = ("lavender", "peach", "flamingo", "sky", "rosewater")

STATE_HUES = {hue for role, hue in MEANINGS.items() if role.startswith("state-")}


def hex_to_rgb(value):
    value = value.lstrip("#")
    return tuple(int(value[i : i + 2], 16) for i in (0, 2, 4))


def rgb_to_hex(rgb):
    return "#" + "".join(f"{round(max(0, min(255, c))):02x}" for c in rgb)


def luminance(value):
    def channel(c):
        c /= 255
        return c / 12.92 if c <= 0.03928 else ((c + 0.055) / 1.055) ** 2.4

    r, g, b = (channel(c) for c in hex_to_rgb(value))
    return 0.2126 * r + 0.7152 * g + 0.0722 * b


def contrast(a, b):
    high, low = sorted((luminance(a), luminance(b)), reverse=True)
    return (high + 0.05) / (low + 0.05)


def mix(a, b, weight):
    """`weight` of `b` into `a`."""
    ra, rb = hex_to_rgb(a), hex_to_rgb(b)
    return rgb_to_hex(tuple(x + (y - x) * weight for x, y in zip(ra, rb)))


def reach(value, toward, background, target):
    """The palette colour, or the least mix toward `toward` that meets `target`."""
    for step in range(0, 101):
        candidate = mix(value, toward, step / 100)
        if contrast(candidate, background) >= target:
            return candidate, step
    raise SystemExit(f"cannot reach {target}:1 from {value} on {background}")


def build():
    palette = json.loads(PALETTE.read_text())
    theme = {"source": f"catppuccin/palette {palette['version']}", "flavours": {}}
    for flavour in FLAVOURS:
        colors = {name: entry["hex"] for name, entry in palette[flavour]["colors"].items()}
        panel, text = colors["base"], colors["text"]
        roles, derived = {}, {}
        for role, name in SURFACES.items():
            roles[role] = colors[name]
        for role, (name, target) in TEXT.items():
            roles[role], step = reach(colors[name], text, panel, target)
            if step:
                derived[role] = f"{name} mixed {step}% toward text"
        for role, name in MEANINGS.items():
            for suffix, target in (("", 3.0), ("-text", 4.5)):
                roles[role + suffix], step = reach(colors[name], text, panel, target)
                if step:
                    derived[role + suffix] = f"{name} mixed {step}% toward text"
        for index, name in enumerate(OWNERSHIP, 1):
            roles[f"own-{index}"], step = reach(colors[name], text, panel, 3.0)
            if step:
                derived[f"own-{index}"] = f"{name} mixed {step}% toward text"
        # A filled accent (primary button): the hue itself on dark flavours; on
        # the light one, its text shade, so light text on it stays legible.
        roles["accent-fill"] = roles["accent"] if flavour in DARK else roles["accent-text"]
        roles["on-accent"] = colors["crust"] if flavour in DARK else colors["base"]
        roles["wash-a"] = colors["teal"]
        roles["wash-b"] = colors["mauve"]
        theme["flavours"][flavour] = {
            "dark": flavour in DARK,
            "name": palette[flavour]["name"],
            "roles": roles,
            "derived": derived,
        }
    return theme


def css(theme):
    lines = [
        "/* Generated by scripts/theme.py from docs/design/catppuccin-palette.json.",
        "   Do not edit: change the mapping in the script and regenerate. */",
    ]
    for flavour, entry in theme["flavours"].items():
        scheme = "dark" if entry["dark"] else "light"
        lines.append(f":root[data-theme=\"{flavour}\"] {{")
        lines.append(f"  color-scheme: {scheme};")
        for role, value in entry["roles"].items():
            lines.append(f"  --{role}: {value};")
        lines.append("}")
    # Before a choice is applied, follow the system: Mocha dark, Latte light.
    mocha, latte = theme["flavours"]["mocha"], theme["flavours"]["latte"]
    for query, entry, scheme in (
        ("(prefers-color-scheme: dark)", mocha, "dark"),
        ("(prefers-color-scheme: light)", latte, "light"),
    ):
        lines.append(f"@media {query} {{")
        lines.append("  :root:not([data-theme]) {")
        lines.append(f"    color-scheme: {scheme};")
        for role, value in entry["roles"].items():
            lines.append(f"    --{role}: {value};")
        lines.append("  }")
        lines.append("}")
    return "\n".join(lines) + "\n"


def check_rules(theme):
    problems = []
    for flavour, entry in theme["flavours"].items():
        roles = entry["roles"]
        for surface in ("s2", "s3"):
            if contrast(roles["text"], roles[surface]) < 4.5:
                problems.append(f"{flavour}: text on {surface}")
        for role in list(MEANINGS) + ["muted", "faint"]:
            name = role if role in ("muted", "faint") else role + "-text"
            if contrast(roles[name], roles["s2"]) < 4.5:
                problems.append(f"{flavour}: {name} below 4.5:1 on s2")
        for role in MEANINGS:
            if contrast(roles[role], roles["s2"]) < 3.0:
                problems.append(f"{flavour}: {role} indicator below 3:1 on s2")
        if contrast(roles["on-accent"], roles["accent-fill"]) < 4.5:
            problems.append(f"{flavour}: on-accent below 4.5:1 on accent-fill")
    if set(OWNERSHIP) & STATE_HUES or set(OWNERSHIP) & {"teal", "mauve"}:
        problems.append("an ownership hue is also a state or accent hue")
    if [hue for role, hue in MEANINGS.items() if hue == "pink" and role != "state-unrecognised"]:
        problems.append("pink is reserved for state-unrecognised")
    return problems


def main():
    theme = build()
    problems = check_rules(theme)
    if problems:
        raise SystemExit("theme rules broken:\n  " + "\n  ".join(problems))
    rendered_json = json.dumps(theme, indent=2) + "\n"
    rendered_css = css(theme)
    if "--check" in sys.argv:
        stale = [
            str(path.relative_to(ROOT))
            for path, content in ((THEME_JSON, rendered_json), (THEME_CSS, rendered_css))
            if not path.exists() or path.read_text() != content
        ]
        if stale:
            raise SystemExit(f"stale generated theme files: {', '.join(stale)}; run scripts/theme.py")
        print("theme: generated files current; contrast and palette rules hold")
        return
    THEME_JSON.write_text(rendered_json)
    THEME_CSS.write_text(rendered_css)
    for flavour, entry in theme["flavours"].items():
        if entry["derived"]:
            print(f"{flavour}: derived {', '.join(entry['derived'])}")
    print(f"wrote {THEME_JSON.relative_to(ROOT)} and {THEME_CSS.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
