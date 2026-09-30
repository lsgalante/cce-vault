#!/usr/bin/env python3
"""Generate a synthetic vault for timing cce-vault (see CLAUDE.md, Performance).

usage: bench/gen-vault.py DIR [NOTES]    (default 5000 notes, ~40 MB)

Each note has frontmatter tags and an alias, 20 random [[Note N]] links, an
inline tag, twelve paragraphs of filler, three tasks and a fenced code block
holding a link that must NOT be indexed. Seeded, so runs are comparable.
"""
import os, random, sys

root = sys.argv[1]
n = int(sys.argv[2]) if len(sys.argv) > 2 else 5000
random.seed(1)
words = ("alpha beta gamma delta rust wayland vulkan note idea project meeting "
         "design canvas graph index link task daily review plan").split()
for i in range(n):
    folder = f"f{i % 40}"
    os.makedirs(f"{root}/{folder}", exist_ok=True)
    links = " ".join(f"[[Note {random.randrange(n)}]]" for _ in range(20))
    body = "\n\n".join(" ".join(random.choice(words) for _ in range(60)) for _ in range(12))
    tasks = "\n".join(f"- [{random.choice(' x')}] {random.choice(words)} {random.choice(words)}"
                      for _ in range(3))
    with open(f"{root}/{folder}/Note {i}.md", "w") as f:
        f.write(f"---\ntags: [{random.choice(words)}]\naliases: [N{i}]\n---\n# Note {i}\n{links}\n"
                f"#{random.choice(words)}\n{body}\n{tasks}\n```\n[[NotALink]]\n```\n")
