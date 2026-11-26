# Claude extension icon

`icon.svg` contains the two paths and view box from
[Beankeeper's canonical favicon](https://beankeeper.app/favicon.svg), retrieved
2026-11-26, with non-rendering provenance metadata omitted. Its view box keeps
the mark centered and padded. `icon.png` renders it at 512×512 on the site's
off-white `#fbfbf9` background.

Regenerate from this directory using librsvg (`rsvg-convert` 2.63.3 was used):

```sh
rsvg-convert --width 512 --height 512 --background-color '#fbfbf9' icon.svg --output icon.png
```

The release tests inspect the icon inside every native `.mcpb` bundle, require
PNG dimensions of at least 256×256, and pin the reviewed PNG's SHA-256. After an
intentional artwork change, inspect the rendered image and update the pinned
hash in `scripts/tests/test_release.py`.
