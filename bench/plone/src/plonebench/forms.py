"""Read the forms and links of a Plone page, as a browser reads them.

Only the standard library. A browser submits every field of a form, with
its current value. So does `Form.fields()`: an edit through the form of
Plone sends every field, not only the field that the user changed.
"""

from __future__ import annotations

import html
import re
from dataclasses import dataclass, field
from html.parser import HTMLParser


@dataclass
class Form:
    action: str
    method: str
    attrs: dict[str, str]
    # (name, value) in the order of the page, as a browser sends them.
    inputs: list[tuple[str, str]] = field(default_factory=list)
    # The submit buttons: name -> value. Only the button that the user
    # clicks goes with the form.
    buttons: dict[str, str] = field(default_factory=dict)

    def fields(self, **changes: str) -> list[tuple[str, str]]:
        """Return the fields to send, with `changes` in place of the values
        of these names. A name in `changes` that the form lacks is added."""
        result = []
        seen = set()
        for name, value in self.inputs:
            if name in changes:
                if name in seen:
                    continue
                seen.add(name)
                result.append((name, changes[name]))
            else:
                result.append((name, value))
        for name, value in changes.items():
            if name not in seen and name not in {n for n, _ in self.inputs}:
                result.append((name, value))
        return result

    def value(self, name: str) -> str | None:
        for key, value in self.inputs:
            if key == name:
                return value
        return None


class _Parser(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.forms: list[Form] = []
        self.links: list[tuple[str, dict[str, str]]] = []
        self._form: Form | None = None
        self._textarea: tuple[str, list[str]] | None = None
        self._select: str | None = None
        self._select_values: list[str] = []
        self._select_first: str | None = None
        self._option: tuple[str | None, bool, list[str]] | None = None

    def handle_starttag(self, tag, attrs):
        a = {k: (v if v is not None else "") for k, v in attrs}
        if tag == "a" and "href" in a:
            self.links.append((a["href"], a))
        if tag == "form":
            self._form = Form(
                action=a.get("action", ""),
                method=a.get("method", "get").lower(),
                attrs=a,
            )
            self.forms.append(self._form)
            return
        form = self._form
        if form is None:
            return
        if tag == "input":
            name = a.get("name")
            kind = a.get("type", "text").lower()
            if not name or "disabled" in a:
                return
            if kind in ("submit", "image"):
                form.buttons[name] = a.get("value", "")
            elif kind in ("checkbox", "radio"):
                if "checked" in a:
                    form.inputs.append((name, a.get("value", "on")))
            elif kind in ("file",):
                return
            elif kind == "button" or kind == "reset":
                return
            else:
                form.inputs.append((name, a.get("value", "")))
        elif tag == "button":
            name = a.get("name")
            if name and a.get("type", "submit").lower() == "submit":
                form.buttons[name] = a.get("value", "")
        elif tag == "textarea":
            name = a.get("name")
            if name and "disabled" not in a:
                self._textarea = (name, [])
        elif tag == "select":
            name = a.get("name")
            if name and "disabled" not in a:
                self._select = name
                self._select_values = []
                self._select_first = None
        elif tag == "option" and self._select is not None:
            self._option = (a.get("value"), "selected" in a, [])

    def handle_data(self, data):
        if self._textarea is not None:
            self._textarea[1].append(data)
        if self._option is not None:
            self._option[2].append(data)

    def handle_endtag(self, tag):
        if tag == "textarea" and self._textarea is not None and self._form is not None:
            name, parts = self._textarea
            text = "".join(parts)
            # A browser drops the first line feed after <textarea>.
            if text.startswith("\n"):
                text = text[1:]
            self._form.inputs.append((name, text))
            self._textarea = None
        elif tag == "option" and self._option is not None:
            value, selected, parts = self._option
            value = value if value is not None else "".join(parts).strip()
            if self._select_first is None:
                self._select_first = value
            if selected:
                self._select_values.append(value)
            self._option = None
        elif tag == "select" and self._select is not None and self._form is not None:
            values = self._select_values
            if not values and self._select_first is not None:
                values = [self._select_first]
            for value in values:
                self._form.inputs.append((self._select, value))
            self._select = None
        elif tag == "form":
            self._form = None


@dataclass
class Page:
    forms: list[Form]
    links: list[tuple[str, dict[str, str]]]

    def form(
        self, *, id: str | None = None, action_contains: str | None = None
    ) -> Form | None:
        for form in self.forms:
            if id is not None and form.attrs.get("id") == id:
                return form
            if action_contains is not None and action_contains in form.action:
                return form
        return None

    def form_with(self, name: str) -> Form | None:
        """Return the first form with a field or a button of this name."""
        for form in self.forms:
            if name in form.buttons or any(key == name for key, _ in form.inputs):
                return form
        return None

    def links_containing(self, text: str) -> list[str]:
        return [href for href, _ in self.links if text in href]


def parse(text: str) -> Page:
    parser = _Parser()
    parser.feed(text)
    parser.close()
    return Page(forms=parser.forms, links=parser.links)


_AUTHENTICATOR = re.compile(
    r'name="_authenticator"\s+value="([^"]+)"|_authenticator=([0-9a-f]+)'
)


def authenticator(text: str) -> str | None:
    """Return the CSRF token of plone.protect in a page, or None."""
    match = _AUTHENTICATOR.search(text)
    if match is None:
        return None
    return html.unescape(match.group(1) or match.group(2))


_CANONICAL = re.compile(r'<link\s+rel="canonical"\s+href="([^"]+)"')


def canonical(text: str) -> str | None:
    match = _CANONICAL.search(text)
    return html.unescape(match.group(1)) if match else None


_STATUS = re.compile(r'class="[^"]*portalMessage[^"]*"[^>]*>(.*?)</div>', re.S)


def status_messages(text: str) -> list[str]:
    return [re.sub(r"<[^>]+>", " ", m).strip() for m in _STATUS.findall(text)]


def is_login_form(text: str) -> bool:
    return 'id="LoginForm"' in text or 'name="__ac_password"' in text
