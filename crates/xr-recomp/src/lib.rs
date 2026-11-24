//! Shared pieces of the static recompiler: the function-table repairs, the CRO module tables, and the
//! generated sources of xgame.dll (lookup tables, CRO modules).
pub mod cro;
pub mod crotab;
pub mod dll;
pub mod repair;
