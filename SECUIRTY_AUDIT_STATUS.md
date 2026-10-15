# Status of Security Audit

## glib < 0.20.0 dependency

Github reports this security vulnerabiity in our BMAC repo, in these files:

- v3/Cargo.lock
- dashboard/Cargo.lock

It's the same issue in both cases: Unsoundness in `Iterator` and `DoubleEndedIterator` impls for `glib::VariantStrIter`

To fix, upgrade glib to version 0.20.0 or later. For example:

```toml
[dependencies]
glib = "0.20.0"
```

```toml
[dev-dependencies]
glib = "0.20.0"
```

Affected versions: >= 0.15.0, < 0.20.0

Patched version: 0.20.0

Full Issue Description:

The VariantStrIter::impl_get function (called internally by implementations of the Iterator and DoubleEndedIterator traits for this type) was unsound, resulting in undefined behaviour.

An immutable reference &p to a *mut libc::c_char pointer initialized to NULL was passed as an argument to a C function that that mutates the pointer behind &p in-place (i.e. as an out-argument), which was unsound. After changes in recent versions of the Rust compiler, these unsound writes through &p now seem to be completely disregarded when building the glib crate with optimizations.

This subsequently caused all calls of VariantStrIter::impl_get to violate the safety requirements of the std::ffi::CStr::from_ptr function - which requires its argument to be a valid pointer to a C-style string - resulting in crashes due to NULL pointer dereferences.

This was fixed by passing the out-argument pointer explitly as &mut p instead of &p.

This issue has been present since this code was initially added in glib v0.15.0. The mismatch in mutability was likely missed (and not raised as an error by the compiler) because the C function wrapped by VariantStrIter::impl_get is variadic (glib_sys::g_variant_get_child), and the pointer in question is one of the variadic arguments.

Why we're delaying changing anything in BMAC for this:

On Linux, Tauri 2 draws its web view with webkit2gtk. Its newest release (2.0.2) requires gtk 0.18 and glib 0.18; there's no newer one.
A gtk 0.19 exists and uses glib 0.22, but webkit2gtk hasn't moved to it, so Tauri (2.12.3 is the latest) is pinned to glib 0.18.5.
The only Tauri that might differ is 3.0.0-alpha, which isn't something to ship.
This applies to both the v3 Deployer and the v2 Dashboard.
Actual exposure

Nothing in either dependency tree calls the broken VariantStrIter except glib itself. That covers our code, Tauri, wry, webkit2gtk and gtk.
The bug is only in the desktop apps, never in bmacd, bmac-backupd or the .deb packages, so the acceptance run doesn't depend on it.

When `webkit2gtk` updates its dependency on gtk, and Tauri 2 pulls in that newer `webkit2gtk` then we'll update BMAC's upstream dependencies to resolve this.