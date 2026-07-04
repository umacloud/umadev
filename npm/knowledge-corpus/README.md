# @umacloud/knowledge

The curated engineering knowledge corpus bundled with
[UmaDev](https://github.com/umacloud/umadev). The main `umadev` package depends
on this one (platform-independent), so the full 400+ file KB ships to every user.

The launcher sets `UMADEV_KNOWLEDGE_DIR` to this directory; the binary retrieves
from it (BM25 + optional local vectors) during the pipeline. A project's own
`knowledge/` folder takes priority, so teams can override or extend it.

Contents (the repo's `knowledge/` tree, copied here by the release CI): standards,
methodologies, expert playbooks, design systems, industry packs, and stack-specific
guides (incl. 微信小程序 / uniapp).
