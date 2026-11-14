"""Write `plone_table.rs`: the tables of the words of the lexicon
`plone_lexicon` of Plone 6.1 (ADR 0060, section 11).

    python plone_table.py OUT.rs

Run it with CPython 3.13, the interpreter of the Plone 6.1 instances of
`just plone`, and Unidecode 1.4.0, the pin of Plone 6.1:

    uv run --no-project --python 3.13 --with Unidecode==1.4.0 \
        python crates/zoe-catalog/src/text/plone_table.py \
        crates/zoe-catalog/src/text/plone_table.rs

The file holds two tables:

- `WORD`: the ranges of the code points that `\\w` of the `re` module
  matches in a str pattern, `str.isalnum()` and `_`, in the Unicode version
  of the interpreter. The splitter of Plone cuts a text at the other code
  points.
- `SECTIONS`: the ASCII that `unidecode` gives for each code point, in
  sections of 256 code points. `baseNormalize` of plone.i18n transliterates
  with it each word that starts with a Latin letter. A code point outside
  the sections, a surrogate and a code point past U+EFFFF give no ASCII, as
  `unidecode` gives none for them.

A release of Plone that changes the pin of Unidecode changes the table: run
this script again with that release.
"""

import sys
import unicodedata
import warnings

import unidecode

# The private lookup of one code point: the ASCII of `unidecode` for a
# character, or None when it has none.
from unidecode import _get_repl_str

PYTHON = ".".join(map(str, sys.version_info[:2]))


def word_ranges():
    """The ranges of the code points of `\\w`, inclusive."""
    ranges = []
    for cp in range(0x110000):
        c = chr(cp)
        if c.isalnum() or c == "_":
            if ranges and ranges[-1][1] + 1 == cp:
                ranges[-1][1] = cp
            else:
                ranges.append([cp, cp])
    return ranges


def sections():
    """The sections of the table: the high bits of the code point and the
    ASCII of its 256 code points."""
    out = {}
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")
        for cp in range(0x80, 0xF0000):
            if 0xD800 <= cp <= 0xDFFF:
                continue
            ascii_ = _get_repl_str(chr(cp))
            if not ascii_:
                continue
            assert ascii_.isascii(), hex(cp)
            out.setdefault(cp >> 8, [""] * 256)[cp & 0xFF] = ascii_
    return sorted(out.items())


def literal(text):
    """`text` as a Rust str literal."""
    out = []
    for c in text:
        if c in ('"', "\\"):
            out.append("\\" + c)
        elif " " <= c <= "~":
            out.append(c)
        else:
            out.append(f"\\x{ord(c):02x}")
    return '"' + "".join(out) + '"'


def main():
    (path,) = sys.argv[1:]
    words = word_ranges()
    table = sections()
    version = getattr(unidecode, "__version__", None)
    if version is None:
        from importlib.metadata import version as dist_version

        version = dist_version("Unidecode")
    lines = [
        "//! The tables of the words of the lexicon `plone_lexicon` of Plone 6.1.",
        "//!",
        f"//! `plone_table.py` generates this file with CPython {PYTHON}",
        f"//! (Unicode {unicodedata.unidata_version}) and Unidecode {version}.",
        "//! Do not edit it: run the generator again.",
        "",
        "/// The ranges of the code points that `\\w` of Python's `re` matches in",
        "/// a str pattern, inclusive and in order.",
        "#[rustfmt::skip]",
        f"pub(super) static WORD: [(u32, u32); {len(words)}] = [",
    ]
    for i in range(0, len(words), 6):
        row = ", ".join(f"(0x{a:x}, 0x{b:x})" for a, b in words[i : i + 6])
        lines.append(f"    {row},")
    lines += [
        "];",
        "",
        "/// The ASCII of `unidecode` for each code point, by section: the code",
        "/// point shifted right by 8 bits, and its 256 code points in order.",
        "#[rustfmt::skip]",
        f"pub(super) static SECTIONS: [(u32, [&str; 256]); {len(table)}] = [",
    ]
    for section, items in table:
        lines.append(f"    (0x{section:x}, [")
        for i in range(0, 256, 16):
            row = ", ".join(literal(t) for t in items[i : i + 16])
            lines.append(f"        {row},")
        lines.append("    ]),")
    lines.append("];")
    with open(path, "w") as out:
        out.write("\n".join(lines) + "\n")
    print(f"{len(words)} ranges of \\w, {len(table)} sections, Unidecode {version}")


if __name__ == "__main__":
    main()
