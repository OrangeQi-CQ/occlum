use std::collections::HashSet;
use crate::process::{do_getegid, do_geteuid, gid_t, uid_t, ThreadRef};
use errno::prelude::*;

bitflags! {
    pub struct IpcFlags: u32 {
        const IPC_CREAT = 0o1000;
        const IPC_EXCL = 0o2000;
    }
}

pub type IpcId = u32;
pub type key_t = u32;
pub type CmdId = u32;

pub const IPC_PRIVATE: key_t = 0;

// For cmd in shmctl(), semctl()
pub const IPC_RMID: CmdId = 0;
pub const IPC_SET: CmdId = 1;
pub const IPC_STAT: CmdId = 2;
pub const IPC_INFO: CmdId = 3;
pub const GETVAL: CmdId = 4;
pub const SETVAL: CmdId = 8;
pub const GETPID: CmdId = 11;
pub const GETNCNT: CmdId = 12;
pub const GETZCNT: CmdId = 13;
pub const GETALL: CmdId = 14;
pub const SETALL: CmdId = 15;

#[allow(non_camel_case_types)]
#[derive(Debug)]
#[repr(C)]
pub(crate) struct ipc_perm_t {
    key: key_t,
    uid: uid_t,
    gid: gid_t,
    cuid: uid_t,
    cgid: gid_t,
    mode: u16,
    pad1: u16,
    seq: u16,
    pad2: u16,
    unused1: u64,
    unused2: u64,
}

#[derive(Debug)]
pub(crate)struct IpcIdManager {
    used_id: HashSet<IpcId>,
    free_num: u32,
    last_alloc_id: IpcId,
}

impl IpcIdManager {
    pub fn new() -> Self {
        let used_id = HashSet::new();
        let free_num = SHMMNI as u32;
        let last_alloc_id = SHMMNI - 1;
        IpcIdManager {
            used_id,
            free_num,
            last_alloc_id,
        }
    }

    // Always return next free id for IpcId
    pub fn get_new_ipcid(&mut self) -> Result<IpcId> {
        if self.free_num == 0 {
            return_errno!(ENOSPC, "all possible shared memory IDs have been taken");
        } else {
            self.free_num -= 1;
        }
        let mut id = self.last_alloc_id + 1;
        loop {
            if id == SHMMNI {
                id = 0;
            }
            if !self.used_id.contains(&id) {
                break;
            }
            id += 1;
        }
        self.last_alloc_id = id;
        Ok(id)
    }

    pub fn free_ipcid(&mut self, ipcid: &IpcId) -> Result<()> {
        self.free_num += 1;
        self.used_id.remove(ipcid);
        Ok(())
    }
}

pub(crate) trait IpcManagerTrait {
    fn current_time() -> u64;
    fn get_new_ipcid(&self) -> Result<IpcId>;
    fn free_ipcid(&self, ipcid: &IpcId) -> Result<()>;
}