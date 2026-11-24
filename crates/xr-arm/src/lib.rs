//! ARMv6K (ARM11 MPCore) instruction decoding for the static recompiler.
pub mod arm;
mod arm_vfp;
pub mod insn;
pub mod thumb;
pub use arm::decode_arm;
pub use thumb::decode_thumb;
pub use insn::*;
