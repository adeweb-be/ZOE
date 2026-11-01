"""The readers of a spike finding, a Markdown document of `docs/spikes`,
that the tests of the spikes share.

pytest puts the directory of each test module on `sys.path`, so a module of
this directory imports this module as `spike_findings`.
"""

import re


def section(text, heading):
    """Return the body of the section `heading`, up to the next section."""
    assert heading in text, f"no section {heading!r}"
    return text.split(heading, 1)[1].split("\n## ", 1)[0]


def cells_of(line):
    """Return the cells of a row of a table, without the backticks."""
    return [
        cell.strip().strip("`").strip() for cell in line.strip().strip("|").split("|")
    ]


def tables(text):
    """Return each table of text as a header and a list of rows."""
    found = []
    lines = text.splitlines()
    index = 0
    while index < len(lines):
        if not lines[index].strip().startswith("|"):
            index += 1
            continue
        block = []
        while index < len(lines) and lines[index].strip().startswith("|"):
            block.append(cells_of(lines[index]))
            index += 1
        header, rows = block[0], block[1:]
        rows = [row for row in rows if not set("".join(row)) <= set("-: ")]
        found.append(([cell.lower() for cell in header], rows))
    return found


def number(text):
    """Return the number of a cell, without its unit."""
    return float(re.sub(r"[^0-9.]", "", text))
