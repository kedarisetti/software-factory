# Knowledge base guide

This folder is a wiki of plain Markdown files, shared by humans and agents.

## Layout
- `<kind>/<topic>/<slug>.md`, kinds: `notes`, `learnings`, `conventions`, `glossary`, `pages`.
- Topic is the task class, else the first tag, else `general`. Glossary may be `glossary/<slug>.md`.
- An entry's ID is its path without `.md`, e.g. `learnings/fix-ci/pin-the-rust-toolchain`.
- `index.md` (root and every folder) lists subfolders and entries with one-line summaries.
- `log.md` is the append-only change log: time, agent, operation, entry.
- `.kb/` is cache and call log; safe to delete.

## Finding things
1. `search` first, using the task's own words (BM25 over title, tags, body).
2. Then `get` the best hits, or `list` a folder (start at the root) to see everything about a topic.
3. Reformulate and search again if the hits are off. Do not read the whole store.

## Writing
- Use `add` for a note, convention or glossary term; `learn` at the end of a run.
- When a topic has many entries, write one synthesis page: kind `page`, with `derived-from`
  links to its sources. One page at a time, no batch rewrites.
- Report whether an entry helped with `feedback`.
- To revise a page, add a new page with `supersedes: <old id>` plus fresh `derived-from` links.
- After editing files by hand, nothing is needed: search picks the change up.

## Rules
- Frontmatter is the source of truth. Links live there; the `[[wikilink]]` footer is generated.
- Never hand-edit `index.md`, `log.md` entries or the links footer; they are regenerated.
- Edit this file freely; it is never overwritten.
