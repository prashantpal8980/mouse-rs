//! Lets the Linux build run on glibc older than the build machine's (B47).
//!
//! glibc 2.43 (Kali, Oct 2026) added new versions of six `float` functions, and a binary
//! linked there asks for `atan2f@GLIBC_2.43` and refuses to start on Ubuntu 24.04 or
//! Mint 22 (glibc 2.39). The original versions are still in every glibc. `build.rs`
//! links with `--wrap=<name>`, which sends every call to `__wrap_<name>` here, and each
//! of those calls the original version, bound by `.symver`. The results differ at most
//! in the last bit: 2.43's versions are the correctly-rounded ones.

core::arch::global_asm!(
    ".symver mouse_rs_old_atan2f, atan2f@GLIBC_2.2.5",
    ".symver mouse_rs_old_acosf, acosf@GLIBC_2.2.5",
    ".symver mouse_rs_old_asinf, asinf@GLIBC_2.2.5",
    ".symver mouse_rs_old_acoshf, acoshf@GLIBC_2.2.5",
    ".symver mouse_rs_old_sinhf, sinhf@GLIBC_2.2.5",
    ".symver mouse_rs_old_coshf, coshf@GLIBC_2.2.5",
);

extern "C" {
    fn mouse_rs_old_atan2f(y: f32, x: f32) -> f32;
    fn mouse_rs_old_acosf(x: f32) -> f32;
    fn mouse_rs_old_asinf(x: f32) -> f32;
    fn mouse_rs_old_acoshf(x: f32) -> f32;
    fn mouse_rs_old_sinhf(x: f32) -> f32;
    fn mouse_rs_old_coshf(x: f32) -> f32;
}

#[no_mangle]
pub extern "C" fn __wrap_atan2f(y: f32, x: f32) -> f32 {
    // SAFETY: a plain libm function of floats, declared with libm's signature.
    unsafe { mouse_rs_old_atan2f(y, x) }
}

#[no_mangle]
pub extern "C" fn __wrap_acosf(x: f32) -> f32 {
    // SAFETY: a plain libm function of floats, declared with libm's signature.
    unsafe { mouse_rs_old_acosf(x) }
}

#[no_mangle]
pub extern "C" fn __wrap_asinf(x: f32) -> f32 {
    // SAFETY: a plain libm function of floats, declared with libm's signature.
    unsafe { mouse_rs_old_asinf(x) }
}

#[no_mangle]
pub extern "C" fn __wrap_acoshf(x: f32) -> f32 {
    // SAFETY: a plain libm function of floats, declared with libm's signature.
    unsafe { mouse_rs_old_acoshf(x) }
}

#[no_mangle]
pub extern "C" fn __wrap_sinhf(x: f32) -> f32 {
    // SAFETY: a plain libm function of floats, declared with libm's signature.
    unsafe { mouse_rs_old_sinhf(x) }
}

#[no_mangle]
pub extern "C" fn __wrap_coshf(x: f32) -> f32 {
    // SAFETY: a plain libm function of floats, declared with libm's signature.
    unsafe { mouse_rs_old_coshf(x) }
}
