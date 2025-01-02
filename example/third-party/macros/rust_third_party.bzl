def third_party_rust_cxx_library(name, **kwargs):
    # @lint-ignore BUCKLINT
    native.cxx_library(name = name, **kwargs)

def third_party_rust_prebuilt_cxx_library(name, **kwargs):
    # @lint-ignore BUCKLINT
    native.prebuilt_cxx_library(name = name, **kwargs)
