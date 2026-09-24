#!/usr/bin/env python3
"""Check the static manual's registry, fragments, routes, and local assets.

Run from any directory: python3 docs/external/check.py
No server or third-party dependencies are required. External URLs are not fetched.
"""

import json
from collections import Counter
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parent


class Document(HTMLParser):
    def __init__(self, source):
        super().__init__(convert_charrefs=True)
        self.ids = []
        self.links = []
        self.assets = []
        self.sections = []
        self.feed(source)

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if attrs.get("id"):
            self.ids.append(attrs["id"])
        if tag == "a" and "href" in attrs:
            self.links.append(attrs["href"])
        if tag in {"script", "img"} and attrs.get("src"):
            self.assets.append(attrs["src"])
        if tag == "link" and attrs.get("href"):
            self.assets.append(attrs["href"])
        if "section" in attrs.get("class", "").split():
            self.sections.append(attrs.get("id"))

    handle_startendtag = handle_starttag


def check():
    errors = []
    pages = json.loads((ROOT / "pages/index.json").read_text())
    registry = {}
    documents = {}
    shell = Document((ROOT / "index.html").read_text())

    for page in pages:
        page_id = page.get("id")
        if not isinstance(page_id, str) or not page_id or any(
            character not in "abcdefghijklmnopqrstuvwxyz0123456789-" for character in page_id
        ):
            errors.append(f"Invalid page id: {page_id!r}")
            continue
        if page_id in registry:
            errors.append(f"Duplicate page id: {page_id}")
        registry[page_id] = page
        for field in ("group", "label", "title", "lede"):
            if not isinstance(page.get(field), str) or not page[field].strip():
                errors.append(f"{page_id}: missing {field}")
        path = ROOT / "pages" / f"{page_id}.html"
        if not path.is_file():
            errors.append(f"{page_id}: missing fragment")
            continue
        document = Document(path.read_text())
        documents[page_id] = document
        if not document.sections or any(not section for section in document.sections):
            errors.append(f"{page_id}: missing section ids")
        for identifier, count in Counter(document.ids + shell.ids).items():
            if count > 1:
                errors.append(f"{page_id}: duplicate DOM id {identifier}")

    for page_id, page in registry.items():
        parent = page.get("parent")
        if parent is not None:
            if parent not in registry or parent == page_id:
                errors.append(f"{page_id}: invalid parent {parent}")
            elif registry[parent]["group"] != page["group"]:
                errors.append(f"{page_id}: parent is in a different navigation group")

    for path in (ROOT / "pages").glob("*.html"):
        if path.stem not in registry:
            errors.append(f"Unregistered fragment: {path.name}")

    route_count = 0
    for name, document in [("index", shell), *documents.items()]:
        for href in document.links + document.assets:
            url = urlsplit(href)
            if url.scheme or url.netloc:
                continue
            if url.path.startswith("/"):
                # Root-relative links are served by the landing site, not this
                # static directory, so they cannot be resolved from disk.
                continue
            if url.path:
                # Fragments are injected into index.html, so their URLs resolve
                # from the site root, not from the pages/ source directory.
                target = ROOT / unquote(url.path).lstrip("/")
                if not target.exists():
                    errors.append(f"{name}: missing local target {href}")
            if url.fragment and url.path in ("", "index.html", "./index.html"):
                route = unquote(url.fragment)
                if name == "index" and route in shell.ids:
                    continue
                route_count += 1
                page_id, _, section_id = route.partition("/")
                if page_id not in documents:
                    errors.append(f"{name}: unknown page route {href}")
                elif section_id and section_id not in documents[page_id].ids:
                    errors.append(f"{name}: unknown section route {href}")

    if errors:
        print("Documentation checks failed:")
        for error in errors:
            print(f"  - {error}")
        return 1
    print(f"Checked {len(documents)} pages, {route_count} internal routes, and local assets.")
    return 0


if __name__ == "__main__":
    raise SystemExit(check())
