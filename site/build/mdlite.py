"""A small Markdown to HTML converter, standard library only.

It covers what the Codehaven docs use: headings, paragraphs, nested lists, fenced
code, inline code, bold, italic, links, tables, blockquotes and rules. It is not a
general CommonMark engine, and it raises nothing for syntax it does not know: that
text comes out as a paragraph.
"""

import html
import re

FENCE = re.compile(r"^ {0,3}(`{3,}|~{3,})\s*([\w+-]*)")
HEADING = re.compile(r"^ {0,3}(#{1,6})\s+(.*?)\s*#*\s*$")
HRULE = re.compile(r"^ {0,3}([-*_])(\s*\1){2,}\s*$")
QUOTE = re.compile(r"^ {0,3}>")
ITEM = re.compile(r"^( *)([-*+]|\d{1,9}[.)])( +|$)(.*)$")
TABLE_SEP = re.compile(r"^\s*\|?\s*:?-{1,}:?\s*(\|\s*:?-{1,}:?\s*)*\|?\s*$")


def slugify(text):
    text = re.sub(r"<[^>]+>", "", text)
    text = html.unescape(text).lower()
    text = re.sub(r"[^\w\s-]", "", text)
    return re.sub(r"[\s]+", "-", text.strip()) or "section"


def plain_text(md):
    """Inline markdown to plain text, for titles and descriptions."""
    md = re.sub(r"\[([^\]]+)\]\([^)]*\)", r"\1", md)
    md = md.replace("`", "")
    md = re.sub(r"(\*\*|__)(.+?)\1", r"\2", md)
    return md.strip()


class Converter:
    def __init__(self, link_fn=None, code_link_fn=None):
        self.link_fn = link_fn or (lambda u: u)
        self.code_link_fn = code_link_fn or (lambda c: None)
        self.headings = []  # (level, id, plain text, inline html)
        self._ids = {}

    # ---------------- inline ----------------

    def inline(self, text):
        stash = []

        def keep(markup):
            stash.append(markup)
            return "\x00%d\x00" % (len(stash) - 1)

        def code_span(m):
            body = m.group(2)
            if len(body) > 1 and body.startswith(" ") and body.endswith(" ") and body.strip():
                body = body[1:-1]
            markup = "<code>%s</code>" % html.escape(body, quote=False)
            target = self.code_link_fn(body.strip())
            if target:
                markup = '<a href="%s">%s</a>' % (html.escape(target), markup)
            return keep(markup)

        text = re.sub(r"(`+)(.+?)(?<!`)\1(?!`)", code_span, text)
        text = html.escape(text, quote=False)

        def link(m):
            label = self.inline_plain_emphasis(m.group(1))
            url = html.unescape(m.group(2))
            href = html.escape(self.link_fn(url))
            external = re.match(r"https?://", url) is not None
            extra = ' rel="noopener"' if external else ""
            return keep('<a href="%s"%s>%s</a>' % (href, extra, label))

        text = re.sub(r"\[([^\]]+)\]\(([^)\s]+)(?:\s+&quot;[^)]*&quot;)?\)", link, text)
        text = self.inline_plain_emphasis(text)

        def restore(t):
            while True:
                new = re.sub(r"\x00(\d+)\x00", lambda m: stash[int(m.group(1))], t)
                if new == t:
                    return t
                t = new

        return restore(text)

    @staticmethod
    def inline_plain_emphasis(text):
        text = re.sub(r"(\*\*|__)(?=\S)(.+?)(?<=\S)\1", r"<strong>\2</strong>", text)
        text = re.sub(r"(?<![\*\w])\*(?=[^\s*])(.+?)(?<=[^\s*])\*(?![\*\w])", r"<em>\1</em>", text)
        text = re.sub(r"(?<![\w])_(?=[^\s_])(.+?)(?<=[^\s_])_(?![\w])", r"<em>\1</em>", text)
        return text

    # ---------------- blocks ----------------

    def convert(self, text):
        lines = text.replace("\t", "    ").split("\n")
        blocks = self.parse_blocks(lines, top=True)
        return "\n".join(b[1] for b in blocks)

    def unique_id(self, text):
        base = slugify(text)
        n = self._ids.get(base, 0)
        self._ids[base] = n + 1
        return base if n == 0 else "%s-%d" % (base, n)

    def is_block_start(self, line, nxt):
        if FENCE.match(line) or HEADING.match(line) or HRULE.match(line) or QUOTE.match(line):
            return True
        m = ITEM.match(line)
        if m and m.group(4).strip():
            marker = m.group(2)
            if marker in "-*+" or marker.startswith("1"):
                return True
        if "|" in line and nxt is not None and TABLE_SEP.match(nxt) and "-" in nxt:
            return True
        return False

    def parse_blocks(self, lines, top=False):
        out = []  # (kind, html, inline_html)
        i = 0
        n = len(lines)
        while i < n:
            line = lines[i]
            if not line.strip():
                i += 1
                continue

            m = FENCE.match(line)
            if m:
                fence, lang = m.group(1), m.group(2)
                i += 1
                body = []
                while i < n:
                    c = re.match(r"^ {0,3}(%s+)\s*$" % re.escape(fence[0]), lines[i])
                    if c and len(c.group(1)) >= len(fence):
                        break
                    body.append(lines[i])
                    i += 1
                i += 1
                cls = ' class="language-%s"' % html.escape(lang) if lang else ""
                code = html.escape("\n".join(body), quote=False)
                out.append(("code", '<pre tabindex="0"><code%s>%s</code></pre>' % (cls, code), None))
                continue

            m = HEADING.match(line)
            if m:
                level = len(m.group(1))
                raw = m.group(2)
                inner = self.inline(raw)
                hid = self.unique_id(plain_text(raw))
                if top:
                    self.headings.append((level, hid, plain_text(raw), inner))
                out.append((
                    "heading",
                    '<h%d id="%s">%s <a class="anchor" href="#%s" aria-label="Link to this section">#</a></h%d>'
                    % (level, hid, inner, hid, level),
                    None,
                ))
                i += 1
                continue

            if HRULE.match(line):
                out.append(("hr", "<hr>", None))
                i += 1
                continue

            if QUOTE.match(line):
                body = []
                while i < n and lines[i].strip() and (QUOTE.match(lines[i]) or not self.is_block_start(lines[i], None)):
                    body.append(re.sub(r"^ {0,3}> ?", "", lines[i]))
                    i += 1
                inner = "\n".join(b[1] for b in self.parse_blocks(body))
                out.append(("quote", "<blockquote>\n%s\n</blockquote>" % inner, None))
                continue

            nxt = lines[i + 1] if i + 1 < n else None
            if "|" in line and nxt is not None and TABLE_SEP.match(nxt) and "-" in nxt:
                i, markup = self.parse_table(lines, i)
                out.append(("table", markup, None))
                continue

            m = ITEM.match(line)
            if m:
                i, markup = self.parse_list(lines, i)
                out.append(("list", markup, None))
                continue

            para = [line.strip()]
            i += 1
            while i < n and lines[i].strip():
                nx = lines[i + 1] if i + 1 < n else None
                if self.is_block_start(lines[i], nx):
                    break
                para.append(lines[i].strip())
                i += 1
            inner = self.inline("\n".join(para))
            out.append(("para", "<p>%s</p>" % inner, inner))
        return out

    # ---------------- tables ----------------

    @staticmethod
    def split_row(row):
        row = row.strip()
        if row.startswith("|"):
            row = row[1:]
        if row.endswith("|") and not row.endswith("\\|"):
            row = row[:-1]
        cells, cur, in_code, i = [], "", 0, 0
        while i < len(row):
            ch = row[i]
            if ch == "\\" and i + 1 < len(row) and row[i + 1] == "|":
                cur += "|"
                i += 2
                continue
            if ch == "`":
                run = len(re.match(r"`+", row[i:]).group(0))
                cur += "`" * run
                i += run
                in_code = 0 if in_code == run else (run if in_code == 0 else in_code)
                continue
            if ch == "|" and not in_code:
                cells.append(cur.strip())
                cur = ""
            else:
                cur += ch
            i += 1
        cells.append(cur.strip())
        return cells

    def parse_table(self, lines, i):
        head = self.split_row(lines[i])
        seps = self.split_row(lines[i + 1])
        aligns = []
        for s in seps:
            if s.startswith(":") and s.endswith(":"):
                aligns.append("center")
            elif s.endswith(":"):
                aligns.append("right")
            else:
                aligns.append(None)
        i += 2
        rows = []
        while i < len(lines) and lines[i].strip() and "|" in lines[i]:
            rows.append(self.split_row(lines[i]))
            i += 1

        def cell(tag, text, k):
            a = aligns[k] if k < len(aligns) else None
            style = ' style="text-align:%s"' % a if a else ""
            scope = ' scope="col"' if tag == "th" else ""
            return "<%s%s%s>%s</%s>" % (tag, scope, style, self.inline(text), tag)

        parts = ['<div class="table-wrap" role="region" aria-label="Scrollable table" tabindex="0"><table>']
        parts.append("<thead><tr>%s</tr></thead>" % "".join(cell("th", t, k) for k, t in enumerate(head)))
        parts.append("<tbody>")
        for r in rows:
            r = (r + [""] * len(head))[: max(len(head), len(r))]
            parts.append("<tr>%s</tr>" % "".join(cell("td", t, k) for k, t in enumerate(r)))
        parts.append("</tbody></table></div>")
        return i, "".join(parts)

    # ---------------- lists ----------------

    def parse_list(self, lines, i):
        n = len(lines)
        first = ITEM.match(lines[i])
        base = len(first.group(1))
        ordered = first.group(2)[0].isdigit()
        start = int(re.match(r"\d+", first.group(2)).group(0)) if ordered else None
        items = []  # list of (lines)
        loose = False
        while i < n:
            m = ITEM.match(lines[i])
            if not m or len(m.group(1)) < base:
                break
            if (m.group(2)[0].isdigit()) != ordered:
                break
            indent = len(m.group(1))
            content_indent = indent + len(m.group(2)) + max(1, len(m.group(3)))
            if len(m.group(3)) > 4:
                content_indent = indent + len(m.group(2)) + 1
            body = [m.group(4)]
            i += 1
            prev_blank = False
            while i < n:
                ln = lines[i]
                if not ln.strip():
                    j = i
                    while j < n and not lines[j].strip():
                        j += 1
                    if j < n and (len(lines[j]) - len(lines[j].lstrip())) >= content_indent:
                        body.extend([""] * (j - i))
                        i = j
                        prev_blank = True
                        continue
                    break
                ind = len(ln) - len(ln.lstrip())
                if ind >= content_indent:
                    body.append(ln[content_indent:])
                    prev_blank = False
                    i += 1
                    continue
                if prev_blank:
                    break
                mm = ITEM.match(ln)
                if mm and len(mm.group(1)) < content_indent:
                    break
                if self.is_block_start(ln, None) or HEADING.match(ln):
                    break
                body.append(ln.strip())  # lazy continuation
                i += 1
            items.append(body)
            # next sibling item?
            j = i
            blank_between = False
            while j < n and not lines[j].strip():
                j += 1
                blank_between = True
            if j < n:
                mm = ITEM.match(lines[j])
                if mm and base <= len(mm.group(1)) < content_indent and (mm.group(2)[0].isdigit()) == ordered:
                    if blank_between:
                        loose = True
                    i = j
                    continue
            break

        rendered = []
        for body in items:
            blocks = self.parse_blocks(body)
            item_loose = loose or any(not b.strip() for b in body)
            inner = []
            for kind, markup, inl in blocks:
                if kind == "para" and not item_loose:
                    inner.append(inl)
                else:
                    inner.append(markup)
            sep = "\n" if any(k != "para" for k, _, _ in blocks) or item_loose else ""
            rendered.append("<li>%s</li>" % "".join(inner) if not sep else "<li>\n%s\n</li>" % "\n".join(inner))
        tag = "ol" if ordered else "ul"
        attr = ' start="%d"' % start if ordered and start != 1 else ""
        return i, "<%s%s>\n%s\n</%s>" % (tag, attr, "\n".join(rendered), tag)
