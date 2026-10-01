//! The sole database process for one application store. Media and Match keep
//! their own transaction gates while sharing a supervised engine owner.
mod client;
mod process;
mod protocol;
mod response;
pub(crate) mod runtime;

pub(crate) use client::{Db, MatchUnitScope};
