load("@prelude//rust:cargo_package.bzl", "cargo")

def cargo_rust_library(name, rustc_flags = [], **kwargs):
    # Always build dependencies with opt-level 1.
    # This disables debug-assertions by default, so re-enable those.
    # You can also do this in toolchains/BUCK by specifying global rustc_flags
    # on toolchains//:rust, but this way leaves our own crates untouched.
    #
    rustc_flags = rustc_flags + select({
        "root//config/mode:debug": ["-Copt-level=1", "-Cdebug-assertions=true"],
        "root//config/mode:release": ["-Copt-level=3"],
    })
    cargo.rust_library(name = name, rustc_flags = rustc_flags, **kwargs)

def third_party_rust_cxx_library(name, **kwargs):
    # @lint-ignore BUCKLINT
    native.cxx_library(name = name, **kwargs)

def third_party_rust_prebuilt_cxx_library(name, **kwargs):
    # @lint-ignore BUCKLINT
    native.prebuilt_cxx_library(name = name, **kwargs)
