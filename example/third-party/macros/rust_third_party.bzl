# @nolint

load(":iterable.bzl", "iterable")
load(":new_sets.bzl", "sets")
load(":type_defs.bzl", "is_dict")

# Get current target platform - hard-coded for example, matches one of the platforms
# defined in reindeer.toml.
def _get_plat():
    return "linux-x86_64"

def extend(orig, new):
    if orig == None:
        ret = new
    elif new == None:
        ret = orig
    elif is_dict(orig):
        ret = orig.copy()
        ret.update(new)
    else:  # list
        ret = orig + new
    return ret

# Add platform-specific args to args for a given platform. This assumes there's some static configuration
# for target platform (_get_plat) which isn't very flexible. A better approach would be to construct
# srcs/deps/etc with `select` to conditionally configure each target, but that's out of scope for this.
def platform_attrs(platformname, platformattrs, attrs):
    for attr in sets.to_list(sets.make(iterable.concat(attrs.keys(), platformattrs.get(platformname, {}).keys()))):
        new = extend(attrs.get(attr), platformattrs.get(platformname, {}).get(attr))
        attrs[attr] = new
    return attrs

def third_party_rust_library(name, platform = {}, dlopen_enable = False, python_ext = None, **kwargs):
    # This works around a bug in Buck, which complains if srcs is missing - but that can happen if all
    # the sources are mapped_srcs
    if "srcs" not in kwargs:
        kwargs["srcs"] = []

    # Rust crates which are python extensions need special handling to make sure they get linked
    # properly. This is not enough on its own - it still assumes there's a dependency on the python
    # library.
    if dlopen_enable or python_ext:
        # This is all pretty ELF/Linux-specific
        linker_flags = ["-shared"]
        if python_ext:
            linker_flags.append("-uPyInit_{}".format(python_ext))
            kwargs["preferred_linkage"] = "static"
        cxx_binary(name = name + "-so", link_style = "static_pic", linker_flags = linker_flags, deps = [":" + name])

    kwargs = platform_attrs(_get_plat(), platform, kwargs)

    rustc_flags = kwargs.get("rustc_flags", [])
    kwargs["rustc_flags"] = ["--cap-lints=allow"] + rustc_flags

    rust_library(name, **kwargs)

# `platform` is a map from a platform (defined in reindeer.toml) to the attributes
# specific to that platform.
def third_party_rust_binary(name, platform = {}, **kwargs):
    # This works around a bug in Buck, which complains if srcs is missing - but that can happen if all
    # the sources are mapped_srcs
    if "srcs" not in kwargs:
        kwargs["srcs"] = []

    kwargs = platform_attrs(_get_plat(), platform, kwargs)

    rustc_flags = kwargs.get("rustc_flags", [])
    kwargs["rustc_flags"] = ["--cap-lints=allow"] + rustc_flags

    rust_binary(name, **kwargs)

def third_party_rust_cxx_library(name, **kwargs):
    cxx_library(name, **kwargs)

def third_party_rust_prebuilt_cxx_library(name, **kwargs):
    prebuilt_cxx_library(name, **kwargs)
