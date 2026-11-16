# Contributing to Pazedo

Thanks for your interest in Pazedo. It is a small project that follows upstream Zed closely, so
a few things matter more than usual.

## Issues

Bug reports and ideas are welcome as GitHub issues. Please include your Pazedo commit, your
Paseo version (`paseo --version`), and whether the daemon runs on the same machine or another
one. Problems that also happen in Zed itself belong in
[Zed's issue tracker](https://github.com/zed-industries/zed/issues).

## Pull requests

- Put Pazedo code in its own crates (`crates/paseo_client`, `crates/paseo_ui`,
  `crates/type_ahead`) and its tooling in `script/pazedo/`.
- Change Zed's own code only where there is no other way. Keep the change small, mark it with a
  `// Pazedo:` comment, and list it under "Upstream touch points" in
  [docs/pazedo/README.md](./docs/pazedo/README.md). Every such change makes merging upstream
  harder.
- Follow the coding guidelines in [.rules](./.rules), which come from Zed.
- Run `cargo test -p paseo_ui -p paseo_client` and `./script/clippy`, and try UI changes in a
  running build. [docs/pazedo/development.md](./docs/pazedo/development.md) explains how to run
  one against a throwaway Paseo daemon, so your real agents stay untouched.
- Describe what you changed and how you checked it.

## License

Pazedo is licensed under GPL-3.0-or-later, like Zed. By contributing, you agree that your
contributions are licensed the same way.
