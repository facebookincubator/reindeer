# Reindeer - Building Cargo Packages with Buck

Jeremy Fitzhardinge <jsgf@fb.com>

This is a set of tools for importing Rust crates from crates.io, git repos, etc
and generating Buck build rules for them. Currently it primarily solves the
problem of managing third-party dependencies in a monorepo built with
[Buck](https://buck.build/), but my hope is that it can be extended to support
[Bazel](https://bazel.build/) and other similar build systems.

## Installation and Building

Reindeer builds with Cargo in the normal way. It has no unusual build-time
dependencies. Therefore, you can use Cargo to not only build Reindeer, but to
install it as well.

```shell
cargo install --locked --git https://github.com/facebookincubator/reindeer reindeer
```

### Nix

If you are using [Nix](https://nixos.org/), you can install Reindeer from
[nixpkgs](https://github.com/NixOS/nixpkgs) via the `reindeer` package. This
package is unofficial and community-maintained.

## Getting started

There's a complete (but small) [example](example) to get started with. More
complete documentation is in [docs](docs/MANUAL.md).

## Contributing

We welcome contributions! See [CONTRIBUTING](CONTRIBUTING.md) for details on how
to get started, and our [code of conduct](CODE_OF_CONDUCT.md).

## License

Reindeer is MIT licensed, as found in the [LICENSE](LICENSE) file.
