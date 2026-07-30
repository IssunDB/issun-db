"""MkDocs hook that puts a "Run in the playground" link under a runnable Cypher block.

A block opts in with an HTML comment on the line before its fence:

    <!-- playground -->
    ```cypher
    MATCH (p:Person) RETURN p.name AS name
    ```

Opt-in rather than every Cypher block, because most of the Cypher in these pages cannot run in
the playground: it binds a query parameter, it is a CLI script rather than Cypher, or it needs
stored embeddings the seeded sample graph does not have. A link landing on an error is worse than
no link, and a marker keeps that judgement next to the example instead of as a list of exceptions
in here. `make playground-check` runs every marked block through the compiled module, so one that
stops working fails the build rather than shipping as a broken link.

The link carries the block as the query, and every earlier marked block on the same page as a
setup script the playground replays first, so a block that builds on one above it still works.
Both travel in the URL fragment, which no server sees.
"""

import base64
import re

# Absolute rather than relative to the page, because the playground is not part of the MkDocs
# build: the docs workflow copies `web/` into `site/playground/` afterwards, so a relative link
# would 404 under `mkdocs serve`.
PLAYGROUND_URL = "https://issundb.github.io/issun-db/playground/"

MARKER = "<!-- playground -->"

MARKED_BLOCK = re.compile(
    r"^<!--[ \t]*playground[ \t]*-->\n(```cypher\n(.*?)^```)$",
    re.DOTALL | re.MULTILINE,
)


def encode(value):
    """URL-safe base64 without padding, which is what the playground's `b64url` decodes."""
    return base64.urlsafe_b64encode(value.encode("utf-8")).decode("ascii").rstrip("=")


def link_for(query, earlier):
    parts = ["q=" + encode(query)]
    if earlier:
        parts.append("s=" + encode(";\n".join(earlier)))
    return PLAYGROUND_URL + "#" + "&".join(parts)


def on_page_markdown(markdown, page, config, files):
    if MARKER not in markdown:
        return markdown

    earlier = []

    def add_link(match):
        fence, query = match.group(1), match.group(2).strip()
        href = link_for(query, earlier)
        earlier.append(query)
        return (
            f"{fence}\n\n"
            f'[Run in the playground]({href}){{target="_blank" rel="noopener"}}\n'
        )

    return MARKED_BLOCK.sub(add_link, markdown)
