#!/usr/bin/env python3
"""Check the generated site. Run from the repository root:

    python3 site/build/check.py

Fails (exit 1) on:
  a) a broken relative link, a missing image, stylesheet, script or icon, or a
     link to an anchor that does not exist;
  b) an em dash, or an en dash used as a dash, in any generated text file;
  c) an absolute http(s) URL in a link or resource attribute other than the
     project's own GitHub and Pages addresses, or in visible text and code other
     than the allowlist below;
  d) an external resource loaded by <script>, <link>, <img>, <source> or CSS.

Standard library only.
"""

import posixpath
import re
import sys
from html.parser import HTMLParser
from pathlib import Path

SITE = Path(__file__).resolve().parents[1]

# Absolute URLs allowed in links and resource attributes (checked by prefix).
LINK_PREFIXES = (
    "https://github.com/aaron777collins/codehaven-forge",
    "https://aaron777collins.github.io/codehaven-forge",
)

# Hosts that may appear as plain text or inside code samples. These are example
# and placeholder addresses used by the docs, plus the two Anthropic hosts the
# connector page names. They are never loaded or linked.
TEXT_HOSTS = {
    "github.com",                # the repository, see LINK_PREFIXES for links
    "aaron777collins.github.io",
    "claude.ai",                 # connector docs: the Claude apps and client metadata
    "claude.com",                # connector docs
    "127.0.0.1",                 # loopback examples
    "localhost",
    "server",                    # compose service name in examples
    "host",                      # placeholder as in https://HOST/nuget/...
    "codehaven_http_bind",       # placeholder in an example setting
}
TEXT_HOST_SUFFIXES = (".example.com", ".example.invalid", ".invalid")
TEXT_HOSTS |= {"example.com", "example.invalid"}

URL_RE = re.compile(r"https?://([^\s\"'<>&)/?#;,\]`]*)", re.I)
LINK_ATTRS = {"a": ["href"], "link": ["href"], "script": ["src"], "img": ["src", "srcset"],
              "source": ["srcset", "src"], "iframe": ["src"], "form": ["action"], "video": ["src", "poster"],
              "audio": ["src"], "object": ["data"], "embed": ["src"]}
RESOURCE_TAGS = {"script", "link", "img", "source", "iframe", "video", "audio", "object", "embed"}

errors = []


def err(where, msg):
    errors.append("%s: %s" % (where, msg))


class Page(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.ids = set()
        self.refs = []   # (tag, attr, value)
        self.text = []

    def handle_starttag(self, tag, attrs):
        a = dict(attrs)
        if "id" in a:
            self.ids.add(a["id"])
        if tag == "a" and "name" in a:
            self.ids.add(a["name"])
        for attr in LINK_ATTRS.get(tag, []):
            if attr in a and a[attr]:
                vals = [v.strip().split(" ")[0] for v in a[attr].split(",")] if attr == "srcset" else [a[attr]]
                for v in vals:
                    self.refs.append((tag, attr, v))
        if tag == "img" and "alt" not in a:
            self.refs.append(("img-no-alt", "alt", ""))

    def handle_data(self, data):
        self.text.append(data)


def is_absolute(v):
    return bool(re.match(r"^([a-z][a-z0-9+.-]*:|//)", v, re.I))


def host_ok(host):
    h = host.lower().split(":")[0].rstrip(".")
    if h == "":
        return True  # a placeholder such as https://<host> in text
    return h in TEXT_HOSTS or h.endswith(TEXT_HOST_SUFFIXES)


def check_dashes(rel, text):
    if "—" in text:
        err(rel, "contains an em dash")
    if re.search(r"\s–|–\s", text):
        err(rel, "contains an en dash used as a dash")


def main():
    files = sorted(p for p in SITE.rglob("*") if p.is_file() and "build" not in p.relative_to(SITE).parts[:1])
    html_files = [p for p in files if p.suffix == ".html"]
    parsed = {}

    for p in files:
        rel = p.relative_to(SITE).as_posix()
        if p.suffix in (".html", ".css", ".js", ".svg", ".xml", ".txt"):
            text = p.read_text(encoding="utf-8")
            check_dashes(rel, text)
        if p.suffix == ".css":
            for m in re.finditer(r"(?:url\(|@import)\s*['\"]?(\S+?)['\"]?\s*\)?\s*[;)]", text):
                if is_absolute(m.group(1)):
                    err(rel, "CSS loads an external resource: %s" % m.group(1))
        if p.suffix == ".js":
            if re.search(r"https?://|\bfetch\(|XMLHttpRequest|\bimport\(", text):
                err(rel, "script makes or names a network request")
        if p.suffix == ".svg" and re.search(r"(href|src)=[\"']https?:", text):
            err(rel, "SVG references an external resource")

    for p in html_files:
        rel = p.relative_to(SITE).as_posix()
        text = p.read_text(encoding="utf-8")
        pg = Page()
        pg.feed(text)
        parsed[rel] = pg
        if "<html" not in text or 'lang="' not in text:
            err(rel, "missing <html lang>")
        if rel != "404.html" and 'id="main"' not in text:
            err(rel, "no #main landmark target")

    for p in html_files:
        rel = p.relative_to(SITE).as_posix()
        pg = parsed[rel]
        for tag, attr, v in pg.refs:
            if tag == "img-no-alt":
                err(rel, "<img> without alt attribute")
                continue
            if is_absolute(v):
                if tag in RESOURCE_TAGS and tag != "link":
                    err(rel, "<%s %s> loads an external resource: %s" % (tag, attr, v))
                    continue
                if tag == "link" and v.rstrip("/") not in LINK_PREFIXES and not v.startswith(LINK_PREFIXES):
                    err(rel, "<link> loads an external resource: %s" % v)
                    continue
                if v.startswith("mailto:"):
                    continue
                if not v.startswith(LINK_PREFIXES):
                    err(rel, "absolute URL not on the allowlist: %s" % v)
                    continue
                # Links to our own Pages address must point at a file we have.
                if v.startswith(LINK_PREFIXES[1]):
                    path = v[len(LINK_PREFIXES[1]):].split("#")[0].lstrip("/") or "index.html"
                    if path.endswith("/"):
                        path += "index.html"
                    if not (SITE / path).is_file():
                        err(rel, "link to our own site is broken: %s" % v)
                continue
            if v.startswith("/"):
                err(rel, "root-relative URL (breaks under a project path): %s" % v)
                continue
            path, _, frag = v.partition("#")
            path = path.split("?")[0]
            if path == "":
                target_rel, target = rel, pg
            else:
                target_rel = posixpath.normpath(posixpath.join(posixpath.dirname(rel), path))
                if target_rel.startswith(".."):
                    err(rel, "link leaves the site: %s" % v)
                    continue
                tp = SITE / target_rel
                if tp.is_dir():
                    target_rel = posixpath.join(target_rel, "index.html")
                    tp = SITE / target_rel
                if not tp.is_file():
                    err(rel, "broken link or missing file (%s %s): %s" % (tag, attr, v))
                    continue
                target = parsed.get(target_rel)
            if frag and target is not None and frag not in target.ids:
                err(rel, "missing anchor #%s in %s" % (frag, target_rel))

        # absolute URLs in visible text and code
        body = "".join(pg.text)
        for m in URL_RE.finditer(body):
            if not host_ok(m.group(1)):
                err(rel, "absolute URL in text, host not on the allowlist: %s" % m.group(0))

    for p in files:
        if p.suffix in (".xml", ".txt"):
            rel = p.relative_to(SITE).as_posix()
            for m in URL_RE.finditer(p.read_text(encoding="utf-8")):
                host = m.group(0).split("//")[1].split("/")[0]
                # www.sitemaps.org is the XML namespace name in sitemap.xml, never fetched.
                if host not in ("aaron777collins.github.io", "www.sitemaps.org"):
                    err(rel, "unexpected URL: %s" % m.group(0))

    for need in (".nojekyll", "404.html", "robots.txt", "sitemap.xml", "favicon.svg", "index.html", "docs/index.html"):
        if not (SITE / need).is_file():
            err(need, "missing")

    print("checked %d html files, %d files in all" % (len(html_files), len(files)))
    if errors:
        for e in errors:
            print("FAIL " + e)
        print("%d problem(s)" % len(errors))
        sys.exit(1)
    print("OK: no broken links, no dashes, no external requests")


if __name__ == "__main__":
    main()
