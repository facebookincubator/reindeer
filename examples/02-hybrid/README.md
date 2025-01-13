# Hybrid Cargo + Buck project

This example is for using both Cargo and Buck on the same project. This is 
useful if you want to:

- use Cargo to do things like `cargo fmt`;
- have it work out of the box with `rust-analyzer`, so it's easy to contribute;
- ultimately build your code with Buck;
- or support building with either.

The key is the `reindeer.toml` in the root, which stitches together a Cargo 
workspace and a Buck package (`third-party`) to buckify Cargo dependencies 
into.

## Running the example

```sh
cargo install --path .
cd examples/02-hybrid

# Note that both buck and cargo can build this project
buck2 run //project:test
cargo run -p project

# Add a dependency if you like
cargo add -p project rand

# Buckify deps. Do this every time you modify the Cargo.tomls, in general.
reindeer buckify

# Modify project/BUCK to add `//third-party:rand` to the deps list.
# Buck should still build the project, and the rand crate should be available
# in main.rs.
```

## Missing from this example

This example does no vendoring or anything fancy with fixups. You will have to 
follow the other examples to get a non-trivial Cargo project built with Buck.

