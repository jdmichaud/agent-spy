# agent-spy

Watches a folder and shows every image created or modified in it, in a window. The most
recent image is displayed, and you can step back through the earlier ones. Handy for keeping
an eye on screenshots or plots that an agent (or anything else) writes to disk.

It talks to the X server directly, so it needs no extra system libraries and works well over
SSH X forwarding. Linux/X11 only (Wayland through XWayland).

## Usage

```
cargo install --path .
agent-spy [OPTIONS] [DIR]
```

`DIR` defaults to the current directory.

| Option              | Effect                                     |
| ------------------- | ------------------------------------------ |
| `-e`, `--existing`  | also list the images already in `DIR`      |
| `-r`, `--recursive` | watch subdirectories too                   |

| Key            | Action                |
| -------------- | --------------------- |
| Left / Right   | previous / next image |
| Home / End     | first / latest image  |
| Esc / q        | quit                  |

The overlay shows each image's modification time, its position in the list and its file name.
PNG, JPEG, GIF, BMP, WebP, TIFF and the other formats supported by the
[image](https://crates.io/crates/image) crate are recognized.

## License

MIT, see [LICENSE](LICENSE).
