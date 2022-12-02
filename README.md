# πνέω

A CLI utility to navigate [INSPIRE](https://inspirehep.net/) and [arXiv](https://arxiv.org/).

Inspired by [spires.app](https://member.ipmu.jp/yuji.tachikawa/spires/).

## How it works

* Write into the text input, arrow keys `🠈 `, `🠊` to move the cursor.
* Arrow keys `🠉 `, `🠋` to select an entry, `Enter` to download/open it.
  * `PgDn`, `PgUp`, mouse scrolling also work for moving, double click works for downloads.
* `Ctrl+R` refreshes the whole screen, in case something interferes with terminal output.
* `Esc` to close dialogs and/or exit.

## Troubleshooting

`pneo` writes logs to `$XDG_RUNTIME_DIR/pneo/pneo.log`. You can increase their verbosity with the
`PNEO_LOG` environment variable, e.g. running with `PNEO_LOG=debug` or `PNEO_LOG=trace`.
