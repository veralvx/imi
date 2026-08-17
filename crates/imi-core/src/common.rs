//! Infrastructure shared by two or more things that are not each other.
//!
//! A module belongs here when it has more than one dependent — whether
//! those are phases or other modules in this tree. A module used by
//! exactly one phase and nothing else lives in that phase's module
//! instead. Keeping the rule mechanical stops phases from reaching into
//! each other for helpers.
//!
//! The second clause is not pedantry. `sysfs` is named directly by only
//! one phase, but also by `mount` and `identity`, which live here;
//! moving it into that phase under a phases-only reading of the rule
//! would force two `common` modules to import from a phase, which is a
//! worse version of the coupling the rule exists to prevent.
//!
//! `testing` is `#[cfg(test)]` and reaches no consumer; it is here for
//! the same reason as the rest, three phase test modules having grown
//! their own copy of one fixture.
//!
//! # The tree is layered, and that is load-bearing
//!
//! `common` does not merely avoid depending on `phases`; it is acyclic
//! internally, in three levels:
//!
//! | level | modules | depends on |
//! | --- | --- | --- |
//! | 0 | `aligned`, `cancel`, `direct_io`, `geometry`, `guard`, `image`, `ioctl`, `sysfs`, `testing` | nothing here |
//! | 1 | `mount`, `throttle`, `identity` | level 0 only |
//! | 2 | `context` | `identity`, `image` |
//!
//! Rust permits circular module dependencies inside a crate, so nothing
//! enforces this — a `sysfs` that reached back into `mount` would
//! compile. The layering is what lets any module here be read without
//! holding the rest in mind, and what keeps `context`, the type every
//! phase signature names, at the top rather than tangled in the middle.
//!
//! Deliberately not enforced by a test: checking it means parsing these
//! files for module paths, and a checker that mistakes a path in a
//! comment or a string for a real dependency reports a cycle that is not
//! there. The table is here to be re-derived by hand when a module gains
//! a dependency, which is rare and always deliberate.
//!
//! Every module here is crate-internal. The types that appear in a
//! phase's public signature are re-exported from the crate root, so a
//! caller reaches `Target` as `imi_core::Target` and never through this
//! module tree — which keeps the layout free to change.

pub(crate) mod aligned;
pub(crate) mod cancel;
pub(crate) mod context;
pub(crate) mod devices;
pub(crate) mod direct_io;
pub(crate) mod geometry;
pub(crate) mod guard;
pub(crate) mod identity;
pub(crate) mod image;
pub(crate) mod ioctl;
pub(crate) mod mount;
pub(crate) mod session;
pub(crate) mod sysfs;
#[cfg(test)]
pub(crate) mod testing;
pub(crate) mod throttle;
