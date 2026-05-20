# @ixla/heso

The agent-native web engine. No Chromium. No Node. One Rust binary.

```sh
npm i -g @ixla/heso
heso open https://example.com
```

Or one-shot:

```sh
npx @ixla/heso open https://example.com
```

This package is the platform-resolver shim. npm installs the right
platform binary as an optional dependency
(`@ixla/heso-<platform>-<arch>`) and the `heso` command on PATH execs
into it.

See [https://github.com/blank3rs/heso](https://github.com/blank3rs/heso) for the source, docs, and full verb reference.

## Platforms

The first release ships Windows x86_64 only. Linux and macOS land
shortly — track progress on [GitHub Releases](https://github.com/blank3rs/heso/releases).

## License

Dual-licensed under MIT and Apache-2.0.
