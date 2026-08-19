//! Reaching a machine over ssh.
//!
//! These endpoints are declared rather than discovered, and the asymmetry with
//! the containers on this machine is worth stating plainly. A container runtime
//! keeps a list of what is running, so reading it is both possible and right.
//! An ssh configuration is a list of what is *configurable* — often hundreds of
//! entries, most of them machines nobody is using today — so connecting to
//! everything in one would be wrong, and rude to whoever is on the other end.
//! Somebody therefore says which machine they want attached, and everything
//! else starts from the words they used to say it.
//!
//! Those words are `ssh`'s, not this program's. Whatever `ssh` accepts is
//! accepted here, and it is `ssh` that says what any of it means; see
//! [`resolve`].

pub mod resolve;

pub use resolve::{Resolved, Resolver, Runner, Ssh};
