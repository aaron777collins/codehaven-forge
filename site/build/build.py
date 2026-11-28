#!/usr/bin/env python3
"""Generate the Codehaven public site into site/ from README.md and the docs.

Run from the repository root:

    python3 site/build/build.py

Standard library only. The output is committed HTML: GitHub Pages serves site/
as it is. Every URL in the output is relative, so the site works under a project
path such as /codehaven-forge/. The build fails if any generated file holds an em
dash or an en dash used as a dash.
"""

import html
import posixpath
import re
import shutil
import sys
from pathlib import Path

sys.dont_write_bytecode = True
HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

from mdlite import Converter, plain_text  # noqa: E402

REPO_ROOT = HERE.parents[1]
OUT = REPO_ROOT / "site"
GITHUB = "https://github.com/aaron777collins/codehaven-forge"
BASE_URL = "https://aaron777collins.github.io/codehaven-forge/"

# Source markdown path (from the repo root) -> published path (from site/).
PAGES = {
    "docs/getting-started.md": "docs/getting-started.html",
    "docs/configuration.md": "docs/configuration.html",
    "docs/accounts.md": "docs/accounts.html",
    "docs/repositories.md": "docs/repositories.html",
    "docs/pull-requests.md": "docs/pull-requests.html",
    "docs/using-ci.md": "docs/using-ci.html",
    "docs/packages.md": "docs/packages.html",
    "docs/pages.md": "docs/pages.html",
    "docs/workspace.md": "docs/workspace.html",
    "docs/admin.md": "docs/admin.html",
    "docs/troubleshooting.md": "docs/troubleshooting.html",
    "docs/faq.md": "docs/faq.html",
    "docs/integration.md": "docs/integration.html",
    "docs/operations.md": "docs/operations.html",
    "docs/git.md": "docs/git.html",
    "docs/web.md": "docs/web.html",
    "docs/collab.md": "docs/collab.html",
    "docs/ci.md": "docs/ci.html",
    "docs/runner.md": "docs/runner.html",
    "docs/terminal.md": "docs/terminal.html",
    "docs/connector.md": "docs/connector.html",
    "docs/foundation.md": "docs/foundation.html",
    "docs/identity.md": "docs/identity.html",
    "docs/state.md": "docs/state.html",
    "ARCHITECTURE.md": "docs/architecture.html",
    "ROADMAP.md": "docs/roadmap.html",
    "docs/design/pages.md": "docs/design/pages.html",
}
# README is not a docs page: its content is the landing page.
LINK_TARGETS = dict(PAGES, **{"README.md": "index.html"})

GROUPS = [
    ("Start here", "Install Codehaven, set it up and keep it running.", [
        ("docs/getting-started.md", "Getting started"),
        ("docs/configuration.md", "Configuration"),
        ("docs/integration.md", "Going public and first run"),
        ("docs/operations.md", "Running an instance"),
    ]),
    ("Use", "Work with accounts, repositories, review, CI, packages and Claude.", [
        ("docs/accounts.md", "Accounts and sign-in"),
        ("docs/repositories.md", "Repositories"),
        ("docs/pull-requests.md", "Issues and pull requests"),
        ("docs/using-ci.md", "Using CI"),
        ("docs/packages.md", "Container registry and packages"),
        ("docs/pages.md", "Pages"),
        ("docs/workspace.md", "The browser workspace"),
        ("docs/terminal.md", "The workspace terminal"),
        ("docs/connector.md", "The Claude connector"),
        ("docs/admin.md", "Site admin"),
    ]),
    ("Help", "When something goes wrong, and quick answers.", [
        ("docs/troubleshooting.md", "Troubleshooting"),
        ("docs/faq.md", "Frequently asked questions"),
    ]),
    ("How it is built", "Notes on each part, the design and the plan.", [
        ("docs/git.md", "Git, SSH and the registry"),
        ("docs/web.md", "The web UI"),
        ("docs/collab.md", "Collaboration"),
        ("docs/ci.md", "CI internals"),
        ("docs/runner.md", "The runner and the CLI"),
        ("docs/foundation.md", "Foundation"),
        ("docs/identity.md", "Identity"),
        ("docs/state.md", "State of the project"),
        ("ARCHITECTURE.md", "Architecture"),
        ("ROADMAP.md", "Roadmap"),
        ("docs/design/pages.md", "Pages design"),
    ]),
]
ORDER = [src for _, _, items in GROUPS for src, _ in items]
LABEL = {src: label for _, _, items in GROUPS for src, label in items}
assert set(ORDER) == set(PAGES), "GROUPS and PAGES disagree"

SCREENSHOTS = [
    # key, label, what it shows
    ("home", "Home", "the home page after sign-in"),
    ("explore", "Explore", "the explore page listing repositories"),
    ("repo-home", "Repository", "a repository home page"),
    ("repo-tree", "File tree", "a repository file tree"),
    ("repo-file", "File view", "a source file in a repository"),
    ("issue", "Issue", "an issue with its discussion"),
    ("pull", "Pull request", "a pull request conversation"),
    ("pull-files", "Pull request files", "the files changed in a pull request"),
    ("actions", "Actions", "the list of workflow runs"),
    ("run", "Workflow run", "one workflow run and its jobs"),
    ("job", "Job log", "a job and its log"),
    ("repo-settings", "Repository settings", "a repository's settings"),
    ("admin", "Site admin", "the site admin area"),
    ("org", "Organization", "an organization page"),
    ("settings-security", "Account security", "the account security settings"),
    ("consent", "Claude connector consent", "the screen where a person chooses what Claude may do"),
    ("connected-apps", "Connected apps", "the list of apps connected to an account"),
    ("run-waiting", "Run waiting for approval", "a workflow run started by a connected app, waiting for approval"),
]
LANDING_SHOTS = ["home", "repo-home", "pull-files", "run", "consent"]
SIZES = {"desktop": (1440, 900), "phone": (390, 844)}

# ---------------------------------------------------------------- helpers

def esc(s):
    return html.escape(s, quote=True)


def rel(target, from_page):
    """Relative URL from one published page to another published path."""
    start = posixpath.dirname(from_page) or "."
    return posixpath.relpath(target, start)


def github_blob(path):
    return "%s/blob/main/%s" % (GITHUB, path.lstrip("./"))


def make_link_fn(src, page):
    src_dir = posixpath.dirname(src)

    def fn(url):
        if re.match(r"^(https?:|mailto:)", url):
            return url
        if url.startswith("#"):
            return url
        path, _, frag = url.partition("#")
        target = posixpath.normpath(posixpath.join(src_dir, path)) if path else src
        suffix = "#" + frag if frag else ""
        if target in LINK_TARGETS:
            return rel(LINK_TARGETS[target], page) + suffix
        return github_blob(target) + suffix

    return fn


def make_code_link_fn(page):
    def fn(text):
        if text in LINK_TARGETS:
            return rel(LINK_TARGETS[text], page)
        return None

    return fn


def first_sentence(md_text):
    """First sentence of the first paragraph after the H1, as plain text."""
    lines = md_text.split("\n")
    i = 0
    while i < len(lines) and not lines[i].startswith("# "):
        i += 1
    i += 1
    while i < len(lines) and not lines[i].strip():
        i += 1
    para = []
    while i < len(lines) and lines[i].strip() and not lines[i].startswith(("#", "```", "|", "-", ">")):
        para.append(lines[i].strip())
        i += 1
    text = plain_text(" ".join(para))
    m = re.match(r"(.+?[.!?])(\s|$)", text)
    text = m.group(1) if m else text
    if len(text) > 190:
        text = text[:187].rsplit(" ", 1)[0] + "..."
    return text


def check_dashes(name, text):
    bad = []
    if "\u2014" in text:
        bad.append("em dash")
    if re.search(r"\s\u2013|\u2013\s", text):
        bad.append("en dash used as a dash")
    if bad:
        raise SystemExit("build failed: %s in %s" % (" and ".join(bad), name))


def write(rel_path, text):
    check_dashes(rel_path, text)
    p = OUT / rel_path
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(text, encoding="utf-8")
    written.append(rel_path)


written = []


# ---------------------------------------------------------------- layout

def layout(page, title, description, body, current=None, body_class=""):
    root = posixpath.relpath(".", posixpath.dirname(page) or ".")
    root = "" if root == "." else root + "/"
    nav = [("docs/index.html", "Docs"), ("screenshots.html", "Screenshots")]
    nav_html = "".join(
        '<a href="%s%s"%s>%s</a>' % (root, href, ' aria-current="page"' if current == href else "", label)
        for href, label in nav
    )
    nav_html += '<a href="%s" rel="noopener">GitHub</a>' % GITHUB
    full_title = title if title == "Codehaven" else "%s | Codehaven" % title
    return """<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<meta name="description" content="{desc}">
<meta name="color-scheme" content="light dark">
<link rel="icon" href="{root}favicon.svg" type="image/svg+xml">
<link rel="stylesheet" href="{root}assets/style.css">
<script src="{root}assets/site.js"></script>
</head>
<body{bodyclass}>
<a class="skip" href="#main">Skip to content</a>
<header class="site-header">
<div class="wrap bar">
<a class="brand" href="{root}index.html"><img src="{root}favicon.svg" alt="" width="26" height="26">Codehaven</a>
<nav class="nav" aria-label="Main">{nav}<button class="theme-btn" id="theme-toggle" type="button" hidden>Dark mode</button></nav>
</div>
</header>
{body}
<footer class="site-footer">
<div class="wrap">
<p>Codehaven is MIT licensed. <a href="{gh}/blob/main/LICENSE" rel="noopener">Read the license</a>.</p>
<p><a href="{gh}" rel="noopener">Source on GitHub</a> &middot; Screenshots show sample data.</p>
</div>
</footer>
</body>
</html>
""".format(
        title=esc(full_title),
        desc=esc(description),
        root=root,
        nav=nav_html,
        body=body,
        gh=GITHUB,
        bodyclass=' class="%s"' % body_class if body_class else "",
    )


# ---------------------------------------------------------------- screenshots

def img_exists(key, size, scheme):
    return (OUT / "img" / ("%s-%s-%s.png" % (key, size, scheme))).is_file()


def picture(page, key, size, alt, sample=True):
    """A <picture> for one shot, or None when no file exists for it."""
    root = posixpath.relpath(".", posixpath.dirname(page) or ".")
    root = "" if root == "." else root + "/"
    light = img_exists(key, size, "light")
    dark = img_exists(key, size, "dark")
    if not (light or dark):
        return None
    w, h = SIZES[size]
    fallback = "light" if light else "dark"
    parts = ["<picture>"]
    if dark:
        parts.append('<source data-scheme="dark" media="(prefers-color-scheme: dark)" srcset="%simg/%s-%s-dark.png">' % (root, key, size))
    if light:
        parts.append('<source data-scheme="light" media="(prefers-color-scheme: light)" srcset="%simg/%s-%s-light.png">' % (root, key, size))
    parts.append(
        '<img src="%simg/%s-%s-%s.png" width="%d" height="%d" loading="lazy" decoding="async" alt="%s">'
        % (root, key, size, fallback, w, h, esc(alt))
    )
    parts.append("</picture>")
    return "".join(parts)


def shot_alt(label, what, size):
    return "Codehaven %s: %s (%s, sample data)" % (label.lower(), what, "phone" if size == "phone" else "desktop")


def figure(page, key, size="desktop"):
    for k, label, what in SCREENSHOTS:
        if k == key:
            pic = picture(page, k, size, shot_alt(label, what, size))
            if not pic:
                return None
            return '<figure class="shot">%s<figcaption>%s (sample data)</figcaption></figure>' % (pic, esc(label))
    raise KeyError(key)


# ---------------------------------------------------------------- landing

def read(path):
    return (REPO_ROOT / path).read_text(encoding="utf-8")


def readme_parts():
    readme = read("README.md")
    flat = re.sub(r"\s+", " ", readme)
    m = re.search(r"A repository starts private; its owner can widen it to every signed-in account on the server, or to anyone on the internet while the site admin allows public repositories\.", flat)
    if not m:
        raise SystemExit("build failed: README.md no longer has the visibility sentence the site quotes")
    visibility = m.group(0)
    m = re.search(r"A self-hosted code forge written in Rust\. (It hosts git over HTTP and SSH.*?container registry\.)", flat)
    if not m:
        raise SystemExit("build failed: README.md no longer has the opening sentences the site quotes")
    hosts = m.group(1)
    start = readme.index("## Quick start")
    end = readme.index("### What goes in")
    quick_md = readme[start:end].split("\n", 1)[1]
    return visibility, hosts, quick_md


def build_landing():
    page = "index.html"
    visibility, hosts, quick_md = readme_parts()
    conv = Converter(link_fn=make_link_fn("README.md", page), code_link_fn=make_code_link_fn(page))
    quick_html = conv.convert(quick_md)

    def doc(src):
        return rel(LINK_TARGETS[src], page)

    hero_shot = figure(page, "home")
    hero_shot_html = '<div class="hero-shot">%s</div>' % hero_shot if hero_shot else ""

    why = [
        ("Self-hosted", "You run it on a Linux machine with Docker. The server is one binary, with Postgres beside it, and everything builds and runs in containers, so the host needs neither Rust nor PostgreSQL."),
        ("Invite-only", "There is no sign-up page. The first account is made on the server with one command, and every account after it comes from an invite."),
        ("Private by default", visibility),
        ("Nothing from another site", "The browser workspace is served entirely by your instance: every file the editor needs comes from it, with nothing loaded from another site."),
    ]
    why_html = "".join('<div class="card"><h3>%s</h3><p>%s</p></div>' % (esc(t), esc(d)) for t, d in why)

    features = [
        ("Git over HTTP and SSH", "Clone, fetch and push over HTTP or SSH.", "docs/repositories.md"),
        ("Issues and pull requests", "Issues, and pull requests with review.", "docs/pull-requests.md"),
        ("CI with runners", "Register a runner and run workflows written in GitHub Actions syntax. Real, unmodified actions from GitHub run on it, such as checkout, setup-node, cache and upload-artifact. A few need things jobs do not get here, and the CI page says which.", "docs/using-ci.md"),
        ("Releases, packages, registry", "Publish releases and packages, and keep container images in a built-in registry.", "docs/packages.md"),
        ("Pages", "Publish a static site from a branch, from its root or its docs folder. A failed copy says why and the last good one keeps serving.", "docs/pages.md"),
        ("Browser workspace", "An editor (Monaco) with a file tree, tabs, search and diff, many-file commits, live workflow runs, and a bash terminal that runs in your browser.", "docs/workspace.md"),
        ("Claude connector", "Connect Claude over OAuth and MCP, within exactly what you allow. It is off until an operator turns it on.", "docs/connector.md"),
        ("Three visibility tiers", "A repository is private, internal or public. Public means anyone, only while the site admin allows public repositories.", "docs/repositories.md"),
        ("Backups", "Back up and restore with one script each, to a directory outside the checkout. Upgrade and rollback are covered too.", "docs/operations.md"),
    ]
    feat_html = "".join(
        '<div class="card"><h3>%s</h3><p>%s</p><a class="more" href="%s">Read more<span class="visually-hidden"> about %s</span></a></div>'
        % (esc(t), esc(d), doc(src), esc(t))
        for t, d, src in features
    )

    gallery = [f for f in (figure(page, k) for k in LANDING_SHOTS) if f]
    gallery_html = ""
    if gallery:
        gallery_html = """<section class="block" id="screenshots" aria-labelledby="shots-h"><div class="wrap">
<h2 id="shots-h">Screenshots</h2>
<p class="sub">Real pages from a test instance, with invented data: the users alice and root, the repository alice/demo and the organization acme.</p>
<div class="shots">%s</div>
<p class="actions"><a class="btn" href="screenshots.html">See all screenshots</a></p>
</div></section>""" % "".join(gallery)

    docs_links = [("docs/getting-started.md", "Getting started"), ("docs/configuration.md", "Configuration"), ("docs/operations.md", "Backup, restore, upgrade"), ("docs/using-ci.md", "CI and runners"), ("docs/troubleshooting.md", "Troubleshooting")]
    docs_html = "".join('<a class="btn" href="%s">%s</a>' % (doc(s), esc(t)) for s, t in docs_links)

    body = """<main id="main">
<section class="hero"><div class="wrap">
<h1>Codehaven</h1>
<p class="tag">A self-hosted code forge written in Rust.</p>
<p class="lede">{hosts}</p>
<div class="actions">
<a class="btn primary" href="#quick-start">Quick start</a>
<a class="btn" href="docs/index.html">Read the docs</a>
<a class="btn" href="{gh}" rel="noopener">View on GitHub</a>
</div>
{hero}
</div></section>

<section class="block" id="why" aria-labelledby="why-h"><div class="wrap">
<h2 id="why-h">Why Codehaven</h2>
<p class="sub">It is made to be run by anyone, on a machine you control.</p>
<div class="grid">{why}</div>
</div></section>

<section class="block" id="features" aria-labelledby="features-h"><div class="wrap">
<h2 id="features-h">What it does</h2>
<p class="sub">One server for the whole loop: push, review, build, release.</p>
<div class="grid">{features}</div>
</div></section>

<section class="block" id="visibility" aria-labelledby="vis-h"><div class="wrap">
<h2 id="vis-h">Who can see a repository</h2>
<p class="sub">{visibility}</p>
<ul class="tiers">
<li><strong>Private</strong>Only people who are granted access.</li>
<li><strong>Internal</strong>Also every signed-in account on the server.</li>
<li><strong>Public</strong>Also anyone on the internet, while the site admin allows public repositories.</li>
</ul>
<p class="note">The tiers are described in <a href="{gitdoc}">the repositories guide</a>. Nobody who is signed out can open a workspace page, even for a public repository.</p>
</div></section>

<section class="block" id="quick-start" aria-labelledby="quick-h"><div class="wrap">
<h2 id="quick-h">Quick start</h2>
<div class="prose">{quick}</div>
<p>The full walk-through, including the proxy, the first admin and the first push, is in <a href="{integ}">Getting started</a>.</p>
</div></section>

{gallery}

<section class="block" id="docs" aria-labelledby="docs-h"><div class="wrap">
<h2 id="docs-h">Documentation</h2>
<p class="sub">Install and run it, use it, fix it, and how it is built.</p>
<div class="actions">{docs_links}<a class="btn" href="docs/index.html">All docs</a></div>
</div></section>

<section class="block" id="license" aria-labelledby="lic-h"><div class="wrap">
<h2 id="lic-h">License and source</h2>
<p>Codehaven is MIT licensed. The source, the issue tracker and the release history are on <a href="{gh}" rel="noopener">GitHub</a>.</p>
</div></section>
</main>""".format(
        hosts=esc(hosts), gh=GITHUB, hero=hero_shot_html, why=why_html, features=feat_html,
        visibility=esc(visibility), gitdoc=doc("docs/repositories.md"), quick=quick_html,
        integ=doc("docs/getting-started.md"), gallery=gallery_html, docs_links=docs_html,
    )
    write(page, layout(page, "Codehaven", "Codehaven is a self-hosted code forge written in Rust: git over HTTP and SSH, issues, pull requests with review, CI, releases, packages and a container registry.", body))


# ---------------------------------------------------------------- screenshots page

def build_screenshots():
    page = "screenshots.html"
    blocks = []
    for key, label, what in SCREENSHOTS:
        d = picture(page, key, "desktop", shot_alt(label, what, "desktop"))
        p = picture(page, key, "phone", shot_alt(label, what, "phone"))
        if not (d or p):
            continue
        cells = ""
        if d:
            cells += '<figure class="shot desktop">%s<figcaption>Desktop (sample data)</figcaption></figure>' % d
        if p:
            cells += '<figure class="shot phone">%s<figcaption>Phone (sample data)</figcaption></figure>' % p
        cls = " has-phone" if (d and p) else ""
        blocks.append('<div class="shot-pair%s" id="%s"><h3>%s</h3>%s</div>' % (cls, esc(key), esc(label), cells))
    if blocks:
        content = "".join(blocks)
    else:
        content = '<p>The screenshots have not been added yet.</p>'
    body = """<main id="main"><div class="wrap prose" style="max-width:none">
<h1>Screenshots</h1>
<p>Real pages from a test instance, in light and dark. They follow your system setting, or the toggle in the header. All of them show invented data: the users alice and root, the repository alice/demo and the organization acme.</p>
{content}
</div></main>""".format(content=content)
    write(page, layout(page, "Screenshots", "Screenshots of Codehaven on desktop and phone, in light and dark, with sample data.", body, current="screenshots.html"))


# ---------------------------------------------------------------- docs

def nav_list(page, current_src):
    parts = []
    for gname, _, items in GROUPS:
        lis = "".join(
            '<li><a href="%s"%s>%s</a></li>'
            % (rel(PAGES[src], page), ' aria-current="page"' if src == current_src else "", esc(label))
            for src, label in items
        )
        parts.append("<h2>%s</h2><ul>%s</ul>" % (esc(gname), lis))
    return "".join(parts)


def toc_list(headings):
    lis = []
    for level, hid, text, inner in headings:
        if level in (2, 3):
            lis.append('<li class="l%d"><a href="#%s">%s</a></li>' % (level, hid, inner))
    return "<ul>%s</ul>" % "".join(lis) if lis else ""


def build_doc(src):
    page = PAGES[src]
    md = read(src)
    conv = Converter(link_fn=make_link_fn(src, page), code_link_fn=make_code_link_fn(page))
    content = conv.convert(md)
    h1s = [h for h in conv.headings if h[0] == 1]
    title = h1s[0][2] if h1s else LABEL[src]
    toc = toc_list(conv.headings)
    idx = ORDER.index(src)
    pager = []
    if idx > 0:
        ps = ORDER[idx - 1]
        pager.append('<a class="prev" href="%s"><small>Previous</small>%s</a>' % (rel(PAGES[ps], page), esc(LABEL[ps])))
    if idx < len(ORDER) - 1:
        ns = ORDER[idx + 1]
        pager.append('<a class="next" href="%s"><small>Next</small>%s</a>' % (rel(PAGES[ns], page), esc(LABEL[ns])))
    docs_index = rel("docs/index.html", page)
    sidebar = nav_list(page, src)
    nav_block = '<nav class="sidebar" aria-label="Documentation"><p style="margin:0 0 0.5rem"><a href="%s">All docs</a></p>%s</nav>' % (docs_index, sidebar)
    menu_block = '<details class="mobile menu"><summary>Docs menu</summary><nav aria-label="Documentation, compact"><p style="margin:0"><a href="%s">All docs</a></p>%s</nav></details>' % (docs_index, sidebar)
    toc_side = '<aside class="toc" aria-label="On this page"><h2>On this page</h2>%s</aside>' % toc if toc else ""
    toc_inline = '<details class="mobile toc-inline"><summary>On this page</summary><div class="toc-list toc">%s</div></details>' % toc if toc else ""
    body = """<div class="wrap docs-grid{has_toc}">
{nav}
<main id="main">
{menu}
{toc_inline}
<article class="prose">
{content}
</article>
<nav class="pager" aria-label="Previous and next page">{pager}</nav>
<p class="source">Source: <a href="{blob}" rel="noopener">{src}</a> in the repository. This page is generated from it.</p>
</main>
{toc_side}
</div>""".format(
        has_toc=" has-toc" if toc else "", nav=nav_block, menu=menu_block, toc_inline=toc_inline,
        content=content, pager="".join(pager), blob=github_blob(src), src=esc(src), toc_side=toc_side,
    )
    desc = first_sentence(md)
    write(page, layout(page, plain_text(title), desc, body, current="docs/index.html"))
    return title, desc


def build_docs_index(descs):
    page = "docs/index.html"
    groups = []
    for gname, gdesc, items in GROUPS:
        cards = "".join(
            '<div class="card"><h3><a href="%s">%s</a></h3><p>%s</p></div>'
            % (rel(PAGES[src], page), esc(label), esc(descs[src]))
            for src, label in items
        )
        groups.append('<section class="group"><h2>%s</h2><p>%s</p><div class="grid">%s</div></section>' % (esc(gname), esc(gdesc), cards))
    body = """<main id="main"><div class="wrap" style="padding-top:2rem;padding-bottom:3rem">
<div class="prose" style="max-width:none">
<h1>Documentation</h1>
<p>One page per area. These pages are generated from the markdown files in the repository, so they match the code they sit beside. The repository is on <a href="{gh}" rel="noopener">GitHub</a>.</p>
</div>
{groups}
</div></main>""".format(gh=GITHUB, groups="".join(groups))
    write(page, layout(page, "Documentation", "Codehaven documentation: start here, use, and concepts.", body, current="docs/index.html"))


# ---------------------------------------------------------------- static files

FAVICON = """<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32" width="32" height="32">
<rect width="32" height="32" rx="7" fill="#0b6b63"/>
<g fill="none" stroke="#ffffff" stroke-width="2.4" stroke-linecap="round">
<path d="M11 9v14"/><path d="M11 17c0-4 10-1 10-6"/>
</g>
<g fill="#0b6b63" stroke="#ffffff" stroke-width="2.4">
<circle cx="11" cy="9" r="2.6"/><circle cx="11" cy="23" r="2.6"/><circle cx="21" cy="11" r="2.6"/>
</g>
</svg>
"""

NOT_FOUND = """<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Page not found | Codehaven</title>
<meta name="robots" content="noindex">
<style>
:root{color-scheme:light dark;--bg:#fbfaf7;--fg:#211f1c;--accent:#0b6b63}
@media (prefers-color-scheme:dark){:root{--bg:#0f1412;--fg:#e8e6e1;--accent:#4fd1c0}}
body{margin:0;background:var(--bg);color:var(--fg);font:1.0625rem/1.65 system-ui,-apple-system,"Segoe UI",Roboto,sans-serif}
main{max-width:34rem;margin:0 auto;padding:4rem 1rem}
h1{font-size:2rem;margin:0 0 .5rem}
a{color:var(--accent)}
a:focus-visible{outline:3px solid var(--accent);outline-offset:2px}
</style>
</head>
<body>
<main id="main">
<h1>Page not found</h1>
<p>That page does not exist on this site.</p>
<p><a href="__BASE__">Go to the Codehaven home page</a> or <a href="__BASE__docs/index.html">read the docs</a>.</p>
</main>
</body>
</html>
""".replace("__BASE__", BASE_URL)


def build_static(sitemap_pages):
    (OUT / "assets").mkdir(parents=True, exist_ok=True)
    for name in ("style.css", "site.js"):
        text = (HERE / name).read_text(encoding="utf-8")
        write("assets/" + name, text)
    write("favicon.svg", FAVICON)
    write("404.html", NOT_FOUND)
    write(".nojekyll", "")
    write("robots.txt", "User-agent: *\nAllow: /\n\nSitemap: %ssitemap.xml\n" % BASE_URL)
    urls = "".join("<url><loc>%s%s</loc></url>\n" % (BASE_URL, "" if p == "index.html" else p) for p in sorted(sitemap_pages))
    write("sitemap.xml", '<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n%s</urlset>\n' % urls)


def main():
    if not (REPO_ROOT / "README.md").is_file():
        raise SystemExit("run from the repository root: python3 site/build/build.py")
    for sub in ("docs", "assets"):
        shutil.rmtree(OUT / sub, ignore_errors=True)
    for f in ("index.html", "screenshots.html", "404.html", "favicon.svg", "robots.txt", "sitemap.xml"):
        (OUT / f).unlink(missing_ok=True)

    descs = {}
    for src in ORDER:
        _, desc = build_doc(src)
        descs[src] = desc
    build_docs_index(descs)
    build_landing()
    build_screenshots()
    pages = [w for w in written if w.endswith(".html") and w != "404.html"]
    build_static(pages)
    print("built %d files into %s" % (len(written), OUT.relative_to(REPO_ROOT)))
    imgs = sorted(p.name for p in (OUT / "img").glob("*.png")) if (OUT / "img").is_dir() else []
    print("screenshots found: %d" % len(imgs))


if __name__ == "__main__":
    main()
