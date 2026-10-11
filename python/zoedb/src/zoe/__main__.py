"""The `zoe` command: the console script `zoe` and `python -m zoe`.

Both run the code of the `zoe` binary, through the extension module.
"""

import signal
import sys

from zoe import _zoe


def main():
    """Run the `zoe` command with the arguments of the process.

    Return the exit code: 0 on success, 1 on a failure, 2 on a usage error.
    """
    # The command runs in Rust without the GIL. Python handles a signal only
    # when it runs Python code, so Ctrl-C would wait for the end of the
    # command. With the default handler, Ctrl-C stops the process, as it
    # stops the binary.
    signal.signal(signal.SIGINT, signal.SIG_DFL)
    return _zoe.main(sys.argv)


if __name__ == "__main__":
    sys.exit(main())
