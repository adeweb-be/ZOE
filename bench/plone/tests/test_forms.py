"""The form reader sends what a browser sends."""

from plonebench import forms

PAGE = """
<html><head><link rel="canonical" href="http://h/Plone/a/b" /></head><body>
<div class="portalMessage statusmessage-info">Info: <span>Changes saved</span></div>
<form id="f" action="http://h/Plone/a/b/@@edit" method="post">
  <input type="text" name="title" value="Old &amp; title" />
  <textarea name="text">
first line</textarea>
  <input type="checkbox" name="flag:list" value="on" checked />
  <input type="checkbox" name="off:list" value="on" />
  <select name="lang"><option value="en">en</option>
    <option value="fr" selected>fr</option></select>
  <select name="first"><option value="x">x</option><option value="y">y</option></select>
  <input type="hidden" name="_authenticator" value="abc123" />
  <input type="submit" name="form.buttons.save" value="Save" />
  <input type="text" name="gone" value="1" disabled />
</form>
<a href="http://h/Plone/a/b/content_status_modify?workflow_action=publish&amp;_authenticator=abc123">P</a>
</body></html>
"""


def test_fields_keep_the_order_and_the_values():
    form = forms.parse(PAGE).form(id="f")
    assert form.inputs == [
        ("title", "Old & title"),
        ("text", "first line"),
        ("flag:list", "on"),
        ("lang", "fr"),
        ("first", "x"),
        ("_authenticator", "abc123"),
    ]
    assert form.buttons == {"form.buttons.save": "Save"}


def test_changes_replace_values_and_add_buttons():
    form = forms.parse(PAGE).form(id="f")
    fields = form.fields(title="New", **{"form.buttons.save": "Save"})
    assert ("title", "New") in fields
    assert ("form.buttons.save", "Save") in fields
    assert ("title", "Old & title") not in fields


def test_page_helpers():
    page = forms.parse(PAGE)
    assert page.form_with("form.buttons.save").action.endswith("@@edit")
    assert page.links_containing("workflow_action=") == [
        "http://h/Plone/a/b/content_status_modify?workflow_action=publish&_authenticator=abc123"
    ]
    assert forms.authenticator(PAGE) == "abc123"
    assert forms.canonical(PAGE) == "http://h/Plone/a/b"
    assert forms.status_messages(PAGE) == ["Info:  Changes saved"]
    assert not forms.is_login_form(PAGE)
