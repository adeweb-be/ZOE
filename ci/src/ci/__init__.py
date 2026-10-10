"""CI as code for ZOE.

Every function runs in a container built from the devenv definition of the
repository. CI therefore uses the same tools as a developer shell.
"""

from .main import Ci as Ci
