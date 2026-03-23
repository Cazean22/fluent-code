# Echo Plugin Example

This directory shows the expected on-disk shape for a tool plugin.

## Layout

```text
echo/
├── guest/
│   ├── Cargo.toml
│   ├── src/
│   │   └── lib.rs
│   └── wit/
│       └── plugin.wit
├── plugin.toml
└── dist/
    └── plugin.wasm
```

`plugin.wasm` is not committed here; build the guest crate and copy the resulting component to `dist/plugin.wasm`.

## Build the sample guest component

The simplest path is to run:

```text
bash ./examples/plugins/echo/build.sh
```

That command builds the guest crate for `wasm32-wasip2` and copies the result to `examples/plugins/echo/dist/plugin.wasm`.

If you prefer to run the steps manually, from `examples/plugins/echo/guest/`:

```text
rustup target add wasm32-wasip2
cargo build --target wasm32-wasip2
```

Then copy the resulting `.wasm` artifact into:

```text
examples/plugins/echo/dist/plugin.wasm
```

The guest crate implements the same `plugin` WIT world that the host expects and exports a single tool named `plugin_echo`.

## Placement

To use this as a project plugin, copy the directory to:

```text
<your-project>/.fluent-code/plugins/echo/
```

To use it as a global plugin, copy it to:

```text
<data_dir>/plugins/echo/
```

The directory is discovered if it contains `plugin.toml`.

## Notes

- The example manifest uses the richer v1 plugin schema.
- It keeps all capabilities at their current safe defaults.
- The tool is configured for manual approval, a 30s timeout, and a 64KiB output limit.
- The guest implementation expects JSON input shaped like `{ "text": "hello" }` and returns the string value unchanged.

See `PLUGIN_AUTHORS.md` for the full field reference and current capability limitations.
