#![deny(clippy::all)]

#[macro_use]
extern crate napi_derive;

#[cfg(
  any(
    all(target_os = "windows", target_arch = "x86_64"), 
    all(target_os = "windows", target_arch = "aarch64")
  )
)]
mod windows;
#[cfg(
  any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "aarch64")
  )
)]
mod linux;
#[cfg(
  any(
    all(target_os = "macos", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
  )
)]
mod macos;
#[cfg(
  not(
    any(
      any(all(target_os = "windows", target_arch = "x86_64"), all(target_os = "windows", target_arch = "aarch64")),
      any(all(target_os = "linux", target_arch = "x86_64"), all(target_os = "linux", target_arch = "aarch64")),
      any(all(target_os = "macos", target_arch = "x86_64"), all(target_os = "macos", target_arch = "aarch64"))
    )
  )
)]
mod unsupported;