# Example Reindeer+Buck project

## Getting Started

The `setup.sh` script will build Reindeer, vendor some Cargo packages and
generate a BUCK file of build rules for them.

This will require a Rust installation (stable, but probably fairly recent), and
Buck to actually make use of the generated files.

## Vendoring

This example enables vendoring of the source files for the third party
crates (see `reindeer.toml` for how). If you run `reindeer vendor` and then
`reindeer buckify`, you get all the crate sources under `third-party/vendor`.
The idea is to check all these files into source control.

Note the file `third-party/.gitignore`. This is used to ignore various
inconvenient files in vendored code. Reindeer itself will look at this to
edit them out of the vendored checksum.json files so that Cargo doesn't
get upset.

There is another option if vendoring would include so many files that it wrecks
your Git performance and you don't have the capacity to tune it:
`vendor = "local-registry"`. This saves tarballs of the crates, instead of
expanding them, and relies on having
[`cargo-local-registry`](https://github.com/dhovart/cargo-local-registry)
installed and Buck2 version `2025-01-15` or later.

## Fixups

This example has a few dependencies to illustrate the fixups system.

### `libc` [toml](third-party/fixups/libc/)

This one is easy. The `libc` crate has a build script, aka `build.rs`, which is
used to enable `--cfg` flags on certain targets.

If you delete the `third-party/fixups/libc` directory, and then `reindeer buckify` again, you'll see a template `fixups.toml` generated, suggesting that
you add some directives to handle the build script. You can then replace the
whole TOML file with:

```toml
[[buildscript]]
[buildscript.rustc_flags]
```

### `crossbeam-utils` [toml](third-party/fixups/crossbeam-utils/)

This is an indirect dependency of `crossbeam-queue`.
It has a build script, which you can see
[here](https://github.com/crossbeam-rs/crossbeam/blob/17fb8417a83a2694b6f8a37198cd20b34b621baf/crossbeam-utils/build.rs).

So we can add a directive to run that build script and have Buck add the flags
it emits to rustc when we compile the crate:

```toml
[[buildscript]]
[buildscript.rustc_flags]
```

However, that build script has the following line:

```rust
                env!("CARGO_PKG_NAME"),
```

Which will fail without some modification. Buck does not provide such
environment variables by default. You can tell reindeer to add them:

```toml
cargo_env = true

[[buildscript]]
[buildscript.rustc_flags]
```

### `blake3` [toml](third-party/fixups/blake3/)

This one is huge, basically replacing a long `build.rs` using the `cc`
crate to invoke a C compiler. The `cc` invocations are manually translated to
`[buildscript.cxx_library]` fixups, which reindeer turns into `cxx_library` targets
for Buck to build. It's more complex still, because the blake3 crate works on
many platforms, so there are many variations of the cxx_library to generate
with different source files and C flags.

If you come across a crate like this, for example `zstd` or `winapi`, you should check
whether someone has already added fixups for it in the [Buck2 repository's fixups
folder](https://github.com/facebook/buck2/tree/main/shim/third-party/rust/fixups).

## Extra BUCK file imports

Reindeer is very customizable, but it can't do everything. This example also
demonstrates adding extra macro imports to the BUCK file, and wrapping the
prelude's rules with your own Starlark code.

A sample use case for this is customizing the opt-levels of third party code by
modifying the `rustc_flags` attribute as it passes through. See the code in

- `config/`, specifying the debug/release constraints
- `PACKAGE`, enabling config modifiers and setting up aliases like `buck2 build -m release`
- `third-party/macros/rust_third_party.bzl`, which defines macros that will be
  imported by `third-party/BUCK`, and selects opt-level based on the
  `//config/mode:debug / release` constraints
- `reindeer.toml`, which tells reindeer to generate a macro call for each crate to use our
  custom macros.

## Reindeer configuration

`reindeer.toml` - Reindeer's configuration. The directory containing this
file is also the base for any relative paths mentioned in the file.

The files and directories Reindeer cares about are under `third-party/`:

- `Cargo.toml` - You edit this to specify which packages you want to import,
  along
  with other settings like features, patches and so on, using the full syntax
  Cargo allows
- `Cargo.lock` - The resolved dependencies
- `BUCK` - The generated Buck build rules (once generated)
- `.gitignore` - This is used to ignore various inconvenient files in vendored
  code. Reindeer itself will look at this to edit them out of the vendored
  checksum.json files so that Cargo doesn't get upset.

## Project

The `project/` directory represents some end-user code. There's nothing notable
about it aside from its references to `//third-party:...` for its third-party
dependencies.

Once everything is set up, you should be able to build it with
`buck build //project:test` to build the executable or `buck run //project:test`
to just build and run in situ.
