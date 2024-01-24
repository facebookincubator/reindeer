# @nolint

def third_party_rust_cxx_library(name, **kwargs):
    native.cxx_library(name = name, **kwargs)

def third_party_rust_prebuilt_cxx_library(name, **kwargs):
    native.prebuilt_cxx_library(name = name, **kwargs)
