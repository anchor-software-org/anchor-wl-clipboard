# anchor-wl-clipboard

Wayland clipboard access for Anchor. It watches and writes clipboard data using
Wayland data-control protocols, with an X11/XWayland fallback when Wayland
clipboard control is unavailable.

It supports UTF-8 text and PNG clipboard data.

## Build

```bash
cargo check
```

## Use

```rust
use anchor_wl_clipboard::{ClipboardEvent, ClipboardWatcherBuilder, ClipboardWriter};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let writer = ClipboardWriter::new()?;
    writer.set_text("Hello from Anchor")?;

    let watcher = ClipboardWatcherBuilder::new().build()?;
    while let Ok(event) = watcher.rx.recv() {
        if let ClipboardEvent::Changed(content) = event {
            println!("{} bytes of {}", content.data.len(), content.mime_type.as_str());
        }
    }
    Ok(())
}
```

The package name is distinct from the upstream `wl-clipboard-rs` dependency
used internally for fallback support.

## License

MIT. See [LICENSE](LICENSE).
