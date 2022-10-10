# XImage Redux
A revived version of GStreamer's ximagesrc, now with resizable window support.

Currently `show-cursor` is not fully implemented due to a crash condition, but PRs to fix this are welcome.

## Usage
### In a Library
Add `gst-plugin-ximageredux` to your `Cargo.toml`, then use the standard GStreamer API.

### CLI
Build the library with `cargo build --release`, then either add the library in `target/release` to your GStreamer plugin path or copy the file to the standard location.
