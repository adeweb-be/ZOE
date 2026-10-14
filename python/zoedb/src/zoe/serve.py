"""`python -m zoe.serve -C FILE`: run `zoe serve` from a `<zoe-server>` section.

The file is a ZConfig file with one `<zoe-server>` section, which
`%import zoe` declares. The keys of the section, except `binary`, are the
options of `zoe serve` without `--`, as in its TOML configuration file.
Each `<storage NAME>` subsection is a `[storages.NAME]` table, with the
keys `path`, `blob-dir` and `read-only`.

The module writes the settings as a TOML file into a memory file, which
has no path on disk, and replaces itself with `zoe serve --config` on
that file. So no Python stays in the process of the server, a supervisor
stops the server with SIGTERM, and no value of the section goes into the
arguments of the process.

The wheel has no server. The binary is the `binary` key of the section,
else `ZOE_BINARY`, else the `zoe` binary on the PATH. The search on the
PATH skips a script, such as the console script `zoe` of the wheel.

An error in the file exits with 2 before the server starts. A missing
binary exits with 1.
"""

import argparse
import io
import os
import sys

import ZConfig
from ZConfig.datatypes import stock_datatypes

from zoe.config import twin

# The exit code of a failure, and of a usage error, as for `zoe`.
FAILURE = 1
USAGE = 2

# The keys of the section that are options of `zoe serve`.
KEYS = (
    "path",
    "blob-dir",
    "listen",
    "admin-listen",
    "invalidation-queue-size",
    "client-queue-size",
    "cache-size",
    "lock-timeout",
    "max-frame-size",
    "shutdown-timeout",
    "idle-timeout",
    "blocking-threads",
    "lock-warn",
    "transaction-warn",
    "log-format",
    "read-only",
    "shared-blob-dir",
    "tls-cert",
    "tls-key",
    "tls-client-ca",
    "tls-read-only-client-ca",
)

# The keys whose value is a boolean.
BOOLEANS = ("read-only", "shared-blob-dir")

# The keys whose leading ~ is the home directory.
PATHS = (
    "path",
    "blob-dir",
    "tls-cert",
    "tls-key",
    "tls-client-ca",
    "tls-read-only-client-ca",
)

SCHEMA = """
<schema>
  <import package="zoe"/>
  <section type="zoe-server" name="*" attribute="server" required="yes"/>
</schema>
"""


class UsageError(Exception):
    """An error in the configuration file."""


def load(url):
    """Return the `<zoe-server>` section of the file or URL `url`."""
    schema = ZConfig.loadSchemaFile(io.StringIO(SCHEMA))
    try:
        config, _ = ZConfig.loadConfig(schema, url)
    except (ZConfig.ConfigurationError, ValueError) as error:
        raise UsageError(str(error)) from None
    return config.server


def toml_string(text):
    """Return `text` as a basic string of TOML."""
    out = ['"']
    for char in text:
        if char in '"\\':
            out.append("\\" + char)
        elif char < " " or char == "\x7f":
            out.append(f"\\u{ord(char):04x}")
        else:
            out.append(char)
    out.append('"')
    return "".join(out)


def settings(section):
    """Return the keys of `zoe serve` that the section sets.

    An empty value counts as not set. `read-only` and `shared-blob-dir`
    become booleans, and the other values stay text, which `zoe serve`
    checks.
    """
    values = {}
    for key in KEYS:
        text = getattr(section, key.replace("-", "_"))
        if not text:
            continue
        if key in BOOLEANS:
            try:
                values[key] = stock_datatypes["boolean"](text)
            except ValueError as error:
                raise UsageError(f"<zoe-server> {key}: {error}") from None
        elif key in PATHS:
            values[key] = os.path.expanduser(text)
        else:
            values[key] = text
    return values


def toml(section):
    """Return the configuration file of `zoe serve` for the section."""
    lines = []
    for key, value in settings(section).items():
        if isinstance(value, bool):
            lines.append(f"{key} = {str(value).lower()}")
        else:
            lines.append(f"{key} = {toml_string(value)}")
    # ZConfig refuses two subsections with the same name.
    for storage in section.storages:
        lines.append("")
        lines.append(f"[storages.{toml_string(storage.getSectionName())}]")
        lines.append(f"path = {toml_string(os.path.expanduser(storage.path))}")
        if storage.blob_dir:
            lines.append(
                f"blob-dir = {toml_string(os.path.expanduser(storage.blob_dir))}"
            )
        lines.append(f"read-only = {str(storage.read_only).lower()}")
    return "\n".join(lines) + "\n"


def environment(section, environ):
    """Return the environment of `zoe serve`.

    It is `environ` without the twin of each key that the section sets, so
    the section wins over the twins, as in the `<zoe>` section.
    """
    given = {twin(key) for key in settings(section)}
    return {name: value for name, value in environ.items() if name not in given}


def is_script(path):
    """Tell if the file `path` starts with `#!`."""
    try:
        with open(path, "rb") as file:
            return file.read(2) == b"#!"
    except OSError:
        return True


def find_binary(section, environ):
    """Return the path of the `zoe` binary, or None.

    A name with a `/` is the path. Another name is searched on the PATH of
    `environ`, and the search skips a script.
    """
    name = os.path.expanduser(section.binary or environ.get("ZOE_BINARY") or "zoe")
    if "/" in name:
        return name
    for directory in environ.get("PATH", os.defpath).split(os.pathsep):
        path = os.path.join(directory or ".", name)
        if os.path.isfile(path) and os.access(path, os.X_OK) and not is_script(path):
            return path
    return None


def main(argv=None):
    """Run `zoe serve` with the `<zoe-server>` section of the file of `-C`.

    On success, the process becomes `zoe serve` and the call does not
    return. Else return the exit code.
    """
    parser = argparse.ArgumentParser(
        prog="python -m zoe.serve",
        description="Run zoe serve with the <zoe-server> section of a ZConfig file.",
    )
    parser.add_argument(
        "-C", dest="file", required=True, metavar="FILE", help="the ZConfig file"
    )
    args = parser.parse_args(argv)
    environ = dict(os.environ)
    try:
        section = load(args.file)
        text = toml(section)
        env = environment(section, environ)
    except UsageError as error:
        print(f"{parser.prog}: {error}", file=sys.stderr)
        return USAGE
    binary = find_binary(section, environ)
    if binary is None:
        print(
            f"{parser.prog}: no zoe binary: give the key binary of <zoe-server>, "
            "or ZOE_BINARY, or put the zoe binary on the PATH",
            file=sys.stderr,
        )
        return FAILURE
    # The memory file stays open in zoe serve, which reads it through its
    # path in /proc.
    descriptor = os.memfd_create("zoe-serve.toml", 0)
    os.write(descriptor, text.encode())
    os.set_inheritable(descriptor, True)
    command = [binary, "serve", "--config", f"/proc/self/fd/{descriptor}"]
    sys.stdout.flush()
    sys.stderr.flush()
    try:
        os.execve(binary, command, env)
    except OSError as error:
        os.close(descriptor)
        print(f"{parser.prog}: cannot run {binary}: {error}", file=sys.stderr)
        return FAILURE


if __name__ == "__main__":
    sys.exit(main())
