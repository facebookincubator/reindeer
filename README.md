# Reindeer - Building Cargo Packages with Buck [![Build Status](https://github.com/facebookincubator/reindeer/actions/workflows/build-and-test.yml/badge.svg)](https://github.com/facebookincubator/reindeer/actions)

Jeremy Fitzhardinge <jsgf@fb.com>

## Project introduction: 
Reindeer is a tool that helps developers get and manage external software packages from the web. 
It automatically configures how these packages are used in projects, making dependency management simple and efficient, especially when dealing with large projects without having to manually set up dependencies.

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

## Example
Here is a simple example of how to import Rust crates and generate Buck build rules using Reindeer:

1. Import a Rust crate 'serde':
    ```shell
   reindeer import serde --version 1.0.0

2. Generate the Buck build rule:
    reindeer generate-buck

3. Verify the generated Buck build:
    once the Buck rule is generated, you can check the Buck file that was created in the appropriate place in the repository.

4. Build the project with Buck: 
    Run the Buck to build the project and make sure serde crate is properly integrated:
    buck build //your_project:your_target


## Contributing

We welcome contributions! See [CONTRIBUTING](CONTRIBUTING.md) for details on how
to get started, and our [code of conduct](CODE_OF_CONDUCT.md).

## License

Reindeer is MIT licensed, as found in the [LICENSE](LICENSE) file.

## Getting Help

If you encounter any issues or have questions, feel free to open an issue in our [GitHub repository](https://github.com/facebookincubator/reindeer/issues) or reach out to the project maintainers.
