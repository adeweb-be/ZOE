"""Read a page of a starting instance as the admin user, and check it.

`python3 read_site.py URL TEXT` reads URL until the instance answers. It
logs in with HTTP basic authentication as the admin user. ZOE_PLONE_ADMIN
gives that user as user:password. Without it, the file that
ZOE_PLONE_ADMIN_FILE names holds that text. The script exits with 0 when the
page has the status 200 and holds TEXT. It exits with an error when the page
has another status, or when the instance does not answer in time.
"""

import base64
import os
import sys
import time
import urllib.error
import urllib.request

# The longest time that the instance takes to start, in seconds.
START_TIME = 300


def admin():
    """Return the name and the password of the admin user."""
    text = os.environ.get("ZOE_PLONE_ADMIN")
    if not text:
        with open(os.environ["ZOE_PLONE_ADMIN_FILE"]) as file:
            text = file.read()
    name, password = text.strip().split(":", 1)
    return name, password


url, text = sys.argv[1:]
token = base64.b64encode(":".join(admin()).encode()).decode()
request = urllib.request.Request(url, headers={"Authorization": f"Basic {token}"})
deadline = time.monotonic() + START_TIME
while True:
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            page = response.read().decode()
        break
    except urllib.error.HTTPError as error:
        sys.exit(f"read_site: {url} gives the status {error.code}")
    except OSError as error:
        # The instance does not listen yet.
        if time.monotonic() > deadline:
            sys.exit(f"read_site: {url} does not answer: {error}")
        time.sleep(1)
if text not in page:
    sys.exit(f"read_site: {url} does not hold {text!r}")
print(f"read_site: {url} gives the status 200 and holds {text!r}")
