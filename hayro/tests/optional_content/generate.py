"""Writes `cases.txt`: one-page PDFs whose content is marked as optional content in
the forms ISO 32000-2 8.11 allows and malformed ones, each with whether PDFium
draws it. `optional_content_reference.rs` checks that hayro draws each as PDFium
does. Groups and membership dictionaries are also streams' dictionaries, marked
content is also begun for a property list there is not, and values are also written as references to nothing and to objects that
play other roles, and lists of groups hold groups written in place, which the
configuration and membership dictionaries may share. A case is a 50 pt page filled green by content marked with `/OC` (a named
`OC` property list of `BDC`, a form or an image XObject), drawn when the centre
pixel is green.

Run: uv run --no-project --with pypdfium2==5.14.0 --with pillow python -I generate.py
"""

import random
import sys
from pathlib import Path

import pypdfium2 as pdfium

SEED = 20261027
COUNT = 3000
OUT = Path(__file__).with_name("cases.txt")


class Doc:
    """Objects from 5 on (1–4 are the catalog, page tree, page and content), and
    entries of the table that lead to another object's body."""

    def __init__(self):
        self.objects = {}
        self.aliases = {}
        self.next = 5

    def new(self, body):
        n = self.next
        self.next += 1
        self.objects[n] = body
        return n

    def reserve(self):
        """An object whose body is written later."""
        return self.new(None)

    def alias(self, target):
        n = self.next
        self.next += 1
        self.aliases[n] = target
        return n

    def file(self, catalog, page, content):
        objects = {1: catalog, 2: "<< /Type /Pages /Kids [3 0 R] /Count 1 >>", 3: page}
        objects[4] = f"<< /Length {len(content)} >>\nstream\n{content}\nendstream"
        objects.update(self.objects)
        out = "%PDF-1.7\n"
        offsets = {}
        for n in sorted(objects):
            offsets[n] = len(out)
            out += f"{n} 0 obj\n{objects[n]}\nendobj\n"
        for n, target in self.aliases.items():
            offsets[n] = offsets[target]
        size = max(offsets) + 1
        xref = len(out)
        out += f"xref\n0 {size}\n0000000000 65535 f \n"
        for n in range(1, size):
            out += f"{offsets[n]:010} 00000 n \n" if n in offsets else "0000000000 65535 f \n"
        out += f"trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
        return out


def pick(r, *weighted):
    """One of `weighted`, pairs of weight and value; a callable value is made
    only when chosen."""
    weighted = [(w, v) for w, v in weighted if w > 0]
    total = sum(w for w, _ in weighted)
    x = r.uniform(0, total)
    chosen = weighted[-1][1]
    for w, v in weighted:
        x -= w
        if x <= 0:
            chosen = v
            break
    return chosen() if callable(chosen) else chosen


DELETED = "9999 0 R"


def case(r):
    doc = Doc()
    # No entry leads to another's body: PDFium reads such an entry as nothing,
    # hayro as the body it leads to, a difference in reading the table that
    # `hayro-write/tests/optional_content.rs` covers.
    #
    # Objects any value may refer to whatever role it plays, so that one object is
    # read in several roles. The first is a usage dictionary, the `/View` of
    # another, or both; the second a list holding groups written in place, which
    # the catalog, the configuration and membership dictionaries name.
    pool = [doc.reserve() for _ in range(6)]
    crossing, shared_list = pool[0], pool[1]

    def value(weighted, nothing=1, shared=1):
        """A value of an entry: one of `weighted`, else a reference to nothing or
        to an object of the pool. One starting with REF is written as an object of
        its own."""
        v = pick(r, (nothing, DELETED), (shared, "POOL"), *weighted)
        if v == "POOL":
            return f"{r.choice(pool)} 0 R"
        if v.startswith("REF"):
            return f"{doc.new(v[3:])} 0 R"
        return v

    def kind(typed, *others):
        """A `/Type` entry: `typed` mostly, else none or another value."""
        v = pick(
            r,
            (12, typed),
            (3, ""),
            (1, "(OCG)"),
            (1, "(OCMD)"),
            (1, "null"),
            (1, "42"),
            (1, DELETED),
            (1, "REF/OCG"),
            *others,
        )
        if v.startswith("REF"):
            v = f"{doc.new(v[3:])} 0 R"
        return f"/Type {v}" if v else ""

    def view_state():
        return value([(6, "/OFF"), (3, "/ON"), (1, "(OFF)"), (1, "/Foo")])

    def view():
        return value(
            [
                (6, lambda: f"<< /ViewState {view_state()} >>"),
                (2, lambda: f"REF<< /ViewState {view_state()} >>"),
                (1, "<< >>"),
                (3, f"{crossing} 0 R"),
            ]
        )

    def usage():
        return value(
            [
                (6, lambda: f"<< /View {view()} >>"),
                (2, lambda: f"REF<< /View {view()} >>"),
                (1, "<< /Print << /PrintState /OFF >> >>"),
                (1, lambda: f"<< /ViewState {view_state()} >>"),
                (4, f"{crossing} 0 R"),
            ]
        )

    def intent():
        return value(
            [
                (2, "/View"),
                (2, "/Design"),
                (1, "[/Design /View]"),
                (1, "/All"),
                (1, "(View)"),
                (1, "[/Design]"),
                (1, "REF[/View]"),
                (1, "REF/Design"),
            ]
        )

    def group_body():
        entries = [kind("/OCG"), f"/Name (G{r.randrange(100)})"]
        if r.random() < 0.4:
            entries.append(f"/Intent {intent()}")
        if r.random() < 0.6:
            entries.append(f"/Usage {usage()}")
        return "<< " + " ".join(e for e in entries if e) + " >>"

    # What the catalog lists as groups, and some membership dictionaries it lists
    # as well, which `/OC` reads as what its `/Type` makes it.
    groups = [doc.reserve() for _ in range(3)]
    null = doc.new("null")

    def group_ref():
        return f"{r.choice(groups)} 0 R"

    def group_list():
        items = []
        for _ in range(r.randrange(0, 4)):
            items.append(
                pick(
                    r,
                    (6, group_ref),
                    (1, DELETED),
                    (1, f"{null} 0 R"),
                    (1, "42"),
                    (2, group_body),
                )
            )
        return "[" + " ".join(items) + "]"

    def groups_named():
        """A list of groups as the configuration names them: by reference, by an
        object of the pool, or a list written in place."""
        return value(
            [(8, lambda: f"[{refs(r, groups)}]"), (2, group_list), (4, f"{shared_list} 0 R")],
            nothing=0,
            shared=2,
        )

    def operator():
        return pick(
            r,
            (9, "/And"),
            (9, "/Or"),
            (9, "/Not"),
            (2, "/Xor"),
            (2, "(And)"),
            (1, ""),
            (1, DELETED),
            (1, lambda: f"{doc.new('/Or')} 0 R"),
        )

    def expression(depth):
        operands = []
        for _ in range(pick(r, (1, 0), (3, 1), (3, 2), (2, 3))):
            operand = pick(
                r,
                (6, "group"),
                (1, "deleted"),
                (1, "null"),
                (1, "number"),
                (1, "direct"),
                (1, "pool"),
                (3 if depth < 3 else 0, "nested"),
                (2 if depth < 3 else 0, "nested ref"),
            )
            if operand == "group":
                operands.append(group_ref())
            elif operand == "deleted":
                operands.append(DELETED)
            elif operand == "null":
                operands.append(f"{null} 0 R")
            elif operand == "number":
                operands.append("42")
            elif operand == "direct":
                operands.append(group_body())
            elif operand == "pool":
                operands.append(f"{r.choice(pool)} 0 R")
            elif operand == "nested":
                operands.append(expression(depth + 1))
            else:
                operands.append(f"{doc.new(expression(depth + 1))} 0 R")
        return "[" + " ".join([operator()] + operands).strip() + "]"

    def membership():
        entries = [kind("/OCMD", (1, "/Bogus"))]
        ocgs = pick(r, (2, None), (3, "single"), (4, "array"), (1, "array ref"), (4, "value"))
        if ocgs == "single":
            entries.append(f"/OCGs {group_ref()}")
        elif ocgs == "value":
            entries.append(f"/OCGs {groups_named()}")
        elif ocgs:
            array = group_list()
            entries.append(
                f"/OCGs {doc.new(array)} 0 R" if ocgs == "array ref" else f"/OCGs {array}"
            )
        if r.random() < 0.7:
            policy = value(
                [
                    (1, "/AllOn"),
                    (1, "/AnyOn"),
                    (1, "/AnyOff"),
                    (1, "/AllOff"),
                    (1, "/Foo"),
                    (1, "(AllOff)"),
                    (1, "REF/AnyOff"),
                ],
                shared=0,
            )
            entries.append(f"/P {policy}")
        ve = pick(r, (3, None), (4, "direct"), (2, "ref"), (1, "not array"), (1, "value"))
        if ve == "direct":
            entries.append(f"/VE {expression(0)}")
        elif ve == "ref":
            entries.append(f"/VE {doc.new(expression(0))} 0 R")
        elif ve == "not array":
            entries.append(f"/VE {group_ref()}")
        elif ve == "value":
            entries.append(f"/VE {value([], nothing=1, shared=3)}")
        return "<< " + " ".join(e for e in entries if e) + " >>"

    for n in groups:
        body = pick(r, (9, group_body), (1, membership))
        doc.objects[n] = streamed(body) if r.random() < 0.08 else body
    doc.objects[crossing] = pick(
        r,
        (2, lambda: f"<< /ViewState {view_state()} >>"),
        (1, lambda: f"<< /View {crossing} 0 R /ViewState {view_state()} >>"),
        (1, lambda: f"<< /View << /ViewState {view_state()} >> /ViewState {view_state()} >>"),
    )
    doc.objects[shared_list] = "[" + " ".join(
        [group_body()] + [group_ref() for _ in range(r.randrange(0, 3))]
    ) + "]"
    for n in pool[2:]:
        doc.objects[n] = pick(
            r,
            (2, lambda: f"<< /ViewState {view_state()} >>"),
            (2, lambda: f"<< /View {view()} >>"),
            (2, lambda: f"<< /View {view()} /ViewState {view_state()} >>"),
            (2, "[/View]"),
            (1, "/View"),
            (1, "/OCG"),
            (4, group_list),
            (1, lambda: f"[{refs(r, groups)}]"),
            (2, lambda: expression(1)),
            (1, membership),
            (2, group_body),
        )

    def oc():
        """An /OC value: a group or membership dictionary, by reference, as the
        dictionary of a stream, or in place."""
        v = pick(r, (4, "group"), (1, "direct group"), (6, "membership"), (1, "pool"))
        if v == "group":
            return group_ref()
        if v == "pool":
            return f"{r.choice(pool)} 0 R"
        body = group_body() if v == "direct group" else membership()
        held = pick(r, (5, "ref"), (1, "stream"), (5, "in place"))
        if held == "in place":
            return body
        return f"{doc.new(streamed(body) if held == 'stream' else body)} 0 R"

    def applications():
        """`/AS`: usage applications, some shared and repeated."""
        made = []
        for _ in range(r.randrange(1, 4)):
            entries = []
            event = pick(r, (6, "/View"), (2, None), (1, "/Print"), (1, "(View)"), (1, DELETED))
            if event:
                entries.append(f"/Event {event}")
            entries.append("/Category [/View]")
            if r.random() < 0.9:
                entries.append(f"/OCGs {groups_named()}")
            if r.random() < 0.9:
                entries.append(f"/View {view()}")
            body = "<< " + " ".join(entries) + " >>"
            made.append(f"{doc.new(body)} 0 R" if r.random() < 0.4 else body)
        items = [r.choice(made) for _ in range(r.randrange(1, 5))]
        return "[" + " ".join(items) + "]"

    def configuration():
        entries = []
        if r.random() < 0.6:
            base = value([(2, "/ON"), (2, "/OFF"), (1, "/Unchanged"), (1, "(OFF)")], shared=0)
            entries.append(f"/BaseState {base}")
        entries.append(f"/ON {groups_named()}")
        entries.append(f"/OFF {groups_named()}")
        if r.random() < 0.25:
            entries.append(f"/AS {applications()}")
        if r.random() < 0.3:
            entries.append(f"/Intent {intent()}")
        return "<< " + " ".join(entries) + " >>"

    # The catalog's /OCProperties.
    listed = [g for g in groups if r.random() < 0.85]
    ocgs = pick(
        r,
        (8, f"[{' '.join(f'{g} 0 R' for g in listed)}]"),
        (1, lambda: f"[{' '.join(f'{g} 0 R' for g in listed)} {group_body()}]"),
        (2, lambda: f"{r.choice(pool)} 0 R"),
        (3, f"{shared_list} 0 R"),
    )
    properties = f"/OCGs {ocgs} /D {configuration()}"
    if r.random() < 0.25:
        configs = [configuration() for _ in range(r.randrange(1, 3))]
        configs = [f"{doc.new(c)} 0 R" if r.random() < 0.5 else c for c in configs]
        properties += f" /Configs [{' '.join(r.choice(configs) for _ in range(r.randrange(1, 4)))}]"
    catalog = "<< /Type /Catalog /Pages 2 0 R"
    if r.random() < 0.95:
        catalog += f" /OCProperties << {properties} >>"
    catalog += " >>"

    fill = "0 1 0 rg 0 0 50 50 re f"
    carrier = pick(
        r,
        (4, "bdc"),
        (1, "bdc span"),
        (3, "form"),
        (2, "image"),
        (1, "form in bdc"),
        (1, "form left open"),
        (1, "mask"),
        (1, "pattern"),
        (1, "type3"),
    )
    resources = ""
    if carrier in ("bdc", "bdc span"):
        tag = "/OC" if carrier == "bdc" else "/Span"
        resources = (
            f"/Properties << /M {oc()} >> "
            "/Font << /F << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >>"
        )
        # Marked content PDFium begins no level for, a property list there is not,
        # whose `EMC` ends the level before it; and text that clips, which clips
        # what follows whether it shows or not, here to a glyph off the page, where
        # the mode at `ET` clips, and once: `Q` brings back no glyph an `ET` used;
        # glyphs without outlines clip all away.
        mode = r.choice([4, 7])
        clipping = pick(
            r,
            (2, f"BT /F 40 Tf {mode} Tr 1 0 0 1 60 60 Tm (A) Tj ET"),
            (1, f"BT /F 40 Tf {mode} Tr 1 0 0 1 60 60 Tm (A) Tj 0 Tr ET"),
            (1, f"BT /F 40 Tf {mode} Tr 1 0 0 1 60 60 Tm (A) Tj q ET Q ET"),
            (1, f"BT /F 40 Tf {mode} Tr 1 0 0 1 10 10 Tm ( ) Tj ET"),
        )
        content = pick(
            r,
            (8, f"{tag} /M BDC {fill} EMC"),
            (1, f"{tag} /M BDC {clipping} EMC {fill}"),
            (1, f"{tag} /M BDC /Span /Missing BDC EMC {fill} EMC"),
            (1, f"{tag} /M BDC /Span << /MCID 0 >> BDC EMC {fill} EMC"),
            (1, f"{tag} /M BDC /OC /Missing BDC {fill} EMC EMC"),
            (1, f"/Span /Missing BDC {tag} /M BDC EMC {fill} EMC"),
        )
    elif carrier == "form left open":
        # A level a form begins and does not end ends with it.
        # A level a form begins and does not end ends with it, and its `EMC` ends
        # none the content drawing it began.
        inside = pick(r, (2, "/OC /N BDC"), (1, "EMC /OC /N BDC"), (1, "EMC EMC /OC /N BDC"))
        form = doc.new(
            f"<< /Type /XObject /Subtype /Form /BBox [0 0 50 50] "
            f"/Resources << /Properties << /N {oc()} >> >> /Length {len(inside)} >>\n"
            f"stream\n{inside}\nendstream"
        )
        resources = f"/XObject << /X {form} 0 R >>"
        content = pick(r, (1, f"/X Do {fill}"), (1, f"/Span BMC /X Do {fill} EMC"))
    elif carrier in ("mask", "pattern"):
        # PDFium draws a soft mask, and the cell of a tiling pattern smaller than
        # what it fills, without optional content.
        inside = pick(
            r,
            (2, "/OC /M BDC 0 1 0 rg 0 0 50 50 re f EMC"),
            (1, "/X Do"),
            (1, "0 1 0 rg 0 0 50 50 re f"),
        )
        own = f"/OC {oc()} " if inside.startswith("0 1") else ""
        nested = doc.new(
            f"<< /Type /XObject /Subtype /Form /BBox [0 0 50 50] /OC {oc()} "
            f"/Length {len(fill)} >>\nstream\n{fill}\nendstream"
        )
        inner = f"/Resources << /Properties << /M {oc()} >> /XObject << /X {nested} 0 R >> >>"
        if carrier == "mask":
            group = doc.new(
                f"<< /Type /XObject /Subtype /Form /BBox [0 0 50 50] {own}"
                f"/Group << /S /Transparency >> {inner} /Length {len(inside)} >>\n"
                f"stream\n{inside}\nendstream"
            )
            resources = f"/ExtGState << /G << /SMask << /S /Alpha /G {group} 0 R >> >> >>"
            content = f"/G gs {fill}"
        else:
            cell = doc.new(
                f"<< /PatternType 1 /PaintType 1 /TilingType 1 /BBox [0 0 10 10] "
                f"/XStep 10 /YStep 10 {inner} /Length {len(inside)} >>\n"
                f"stream\n{inside}\nendstream"
            )
            resources = f"/Pattern << /P {cell} 0 R >>"
            content = "/Pattern cs /P scn 0 0 50 50 re f"
    elif carrier == "type3":
        # PDFium draws a Type 3 glyph with the optional content of the text.
        glyph = "1000 0 d0 /OC /M BDC 0 1 0 rg 0 0 1000 1000 re f EMC"
        proc = doc.new(f"<< /Length {len(glyph)} >>\nstream\n{glyph}\nendstream")
        font = doc.new(
            "<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] "
            "/FontMatrix [0.001 0 0 0.001 0 0] /FirstChar 97 /LastChar 97 /Widths [1000] "
            "/Encoding << /Differences [97 /a] >> "
            f"/CharProcs << /a {proc} 0 R >> /Resources << /Properties << /M {oc()} >> >> >>"
        )
        resources = f"/Font << /T {font} 0 R >>"
        content = "BT /T 50 Tf (a) Tj ET"
    elif carrier == "image":
        image = doc.new(
            "<< /Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceRGB "
            f"/BitsPerComponent 8 /Filter /ASCIIHexDecode /OC {oc()} /Length 7 >>\n"
            "stream\n00FF00>\nendstream"
        )
        resources = f"/XObject << /X {image} 0 R >>"
        content = "q 50 0 0 50 0 0 cm /X Do Q"
    else:
        # A form's content ends no level the content drawing it begins.
        inside = pick(r, (6, fill), (1, f"EMC {fill}"), (1, f"/OC /Missing BDC EMC {fill}"))
        form = doc.new(
            f"<< /Type /XObject /Subtype /Form /BBox [0 0 50 50] /OC {oc()} "
            f"/Length {len(inside)} >>\nstream\n{inside}\nendstream"
        )
        resources = f"/XObject << /X {form} 0 R >>"
        content = "/X Do"
        if carrier == "form in bdc":
            resources += f" /Properties << /M {oc()} >>"
            content = f"/OC /M BDC {content} EMC"
    page = (
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 50 50] "
        f"/Resources << {resources} >> /Contents 4 0 R >>"
    )
    return doc.file(catalog, page, content)


def streamed(body):
    """A dictionary written as that of a stream, whose data are empty or, kept in
    another file (/F), cannot be read."""
    if len(body) % 23 == 0:
        return body[: -len(">>")] + "/F (elsewhere) /Length 1 >>\nstream\nx\nendstream"
    return body[: -len(">>")] + "/Length 0 >>\nstream\n\nendstream"


def refs(r, items):
    return " ".join(f"{n} 0 R" for n in items if r.random() < 0.5)


def drawn(pdf):
    page = pdfium.PdfDocument(pdf.encode("latin-1"))[0]
    pixel = page.render(scale=1).to_pil().convert("RGB").getpixel((25, 25))
    return pixel[1] > 200 and pixel[0] < 50


def main():
    r = random.Random(SEED)
    pdfs = [case(r) for _ in range(COUNT)]
    # Content in an OCMD written in place as the operand of BDC, which PDFium does
    # not read as optional content.
    inline = Doc()
    inline.new("<< /Type /OCG /Name (Off) >>")
    pdfs.append(
        inline.file(
            "<< /Type /Catalog /Pages 2 0 R "
            "/OCProperties << /OCGs [5 0 R] /D << /OFF [5 0 R] >> >> >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 50 50] /Contents 4 0 R >>",
            "/OC << /Type /OCMD /OCGs 5 0 R >> BDC 0 1 0 rg 0 0 50 50 re f EMC",
        )
    )
    out = f"# {COUNT + 1} cases, seed {SEED}, {pdfium.PDFIUM_INFO}\n"
    for i, pdf in enumerate(pdfs):
        out += f"=== {i} {int(drawn(pdf))} {len(pdf)}\n{pdf}\n"
    OUT.write_text(out, encoding="latin-1")
    shown = sum(1 for line in out.splitlines() if line.startswith("=== ") and line.split()[2] == "1")
    print(f"{len(pdfs)} cases, {shown} drawn, {len(out)} bytes", file=sys.stderr)


main()
