# Reindeer - Third-party crate management for Rust

Jeremy Fitzhardinge <jsgf@fb.com>

This is a set of tools for importing Rust crates from crates.io, git repos,
etc and generating Buck build rules for them. Currently it primarily solves
the problem of managing third-party dependencies in a monorepo built with
[Buck](https://buck.build/), but my hope is that it can be extended to support
[Bazel](https://bazel.build/) and other similar build systems.

## Getting started

There's a complete (but small) [example](example) tp get started with. More complete
documentation is in [docs](docs/).

## Building

Reindeer builds with Cargo in the normal way. It has no unusual build-time dependencies.

## Contributing

We welcome contributions! See [CONTRIBUTING](CONTRIBUTING.md) for details on how to get started, and our [code of conduct](CODE_OF_CONDUCT.md).

## License

Reindeer is MIT licensed, as found in the [LICENSE](LICENSE) file.
