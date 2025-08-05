use super::*;

mod ipc_util;
mod shm;
mod sem;
mod syscalls;

pub use self::ipc_util::{IpcId, key_t, CmdId};
pub use self::shm::{shmids_t, SYSTEM_V_SHM_MANAGER};
pub use self::sem::*;
pub use self::syscalls::{do_shmat, do_shmctl, do_shmdt, do_shmget};