use super::*;

use crate::ipc::ipc_util::*;
use crate::process::{do_getegid, do_geteuid, gid_t, uid_t, pid_t, ThreadRef};
use crate::time::{do_gettimeofday, time_t};
use crate::util::mem_util::from_user;
use bitflags::bitflags;
use core::sync::atomic::AtomicI32;
use std::collections::{HashMap, HashSet};
use crate::events::{Observer, Waiter, WaiterQueue};
use alloc::vec::Vec;
use std::cmp::Ordering;

#[allow(non_camel_case_types)]
pub type SemId = IpcId;

// Maximum number of semaphore sets
const SEMMNI: SemId = 128;
// Maximum semaphores per set
const SEMMSL: usize = 250;
// Maximum semaphores system-wide
const SEMMNS: usize = SEMMNI as usize * SEMMSL;
// Maximum operations per semop call
const SEMOPM: usize = 32;


bitflags! {
    pub struct SemFlags: u32 {
        const IPC_CREAT = 0o1000;
        const IPC_EXCL = 0o2000;
    }
}

#[allow(non_camel_case_types)]
#[derive(Debug)]
#[repr(C)]
pub struct semids_t {
    sem_perm: ipc_perm_t,
    sem_otime: time_t,
    sem_ctime: time_t,
    sem_nsems: u64,
    unused1: u64,
    unused2: u64,
}

#[allow(non_camel_case_types)]
#[derive(Debug)]
#[repr(C)]
pub struct sembuf_t {
    sem_num: u16,
    sem_op: i16,
    sem_flg: i16,
}

#[derive(Debug, Clone)]
struct Semaphore {
    count: i32,
    waiter_queue: WaiterQueue,
    last_pid: pid_t,
}

impl Semaphore {
    fn new(initial_value: i32) -> Self {
        Semaphore {
            count: initial_value,
            waiter_queue: WaiterQueue::new(),
            last_pid: current!().process().pid(),
        }
    }

    fn try_wake(&mut self) {
        while self.value > 0 && !self.waiter_queue.is_empty() {
            self.waiter_queue.dequeue_and_wake_one();
            self.value -= 1;
        }
    }

    fn wait(&mut self) {
        let waiter = Waiter::new();
        self.waiter_queue.reset_and_enqueue(waiter);
    }
}

#[derive(Debug)]
struct SemSet {
    semid: SemId,
    key: key_t,
    nsems: usize,

    uid: uid_t,
    gid: gid_t,
    cuid: uid_t,
    cgid: gid_t,
    mode: u16,

    sem_otime: time_t,
    sem_ctime: time_t,

    sems: Vec<Semaphore>,
    attached_pids: HashSet<pid_t>,
}

impl SemSet {
    fn new(semid: SemId, key: key_t, nsems: usize, mode: u16) -> Result<Self> {
        if nsems == 0 || nsems > SEMMSL {
            return_errno!(EINVAL, "invalid number of semaphores");
        }

        let sems = (0..nsems)
            .map(|_| Semaphore::new(0))
            .collect();

        Ok(SemSet {
            semid,
            key,
            nsems,
            uid: do_geteuid().unwrap() as u32,
            cuid: do_geteuid().unwrap() as u32,
            gid: do_getegid().unwrap() as u32,
            cgid: do_getegid().unwrap() as u32,
            mode,
            sem_otime: 0,
            sem_ctime: SemManager::current_time(),
            sems,
            attached_pids: HashSet::new(),
        })
    }

    fn check_perm(&self) -> Result<()> {
        // TODO: Implement proper permission checks
        Ok(())
    }

    fn attach_pid(&mut self, pid: pid_t) {
        self.attached_pids.insert(pid);
    }

    fn detach_pid(&mut self, pid: &pid_t) {
        self.attached_pids.remove(pid);
    }

    fn do_semop(&mut self, sops: &[sembuf_t]) -> Result<()> {
        let current_pid = current!().process().pid();
        let current_thread = current!();

        // Validate all operations first
        for sop in sops {
            if sop.sem_num as usize >= self.nsems {
                return_errno!(EFBIG, "semaphore number out of range");
            }
        }

        // Process each operation
        for sop in sops {
            let sem_num = sop.sem_num as usize;
            let sem = &mut self.sems[sem_num];
            
            match sop.sem_op.cmp(&0) {
                Ordering::Greater => {
                    // Increment operation
                    sem.value += sop.sem_op as i32;
                    sem.try_wake();
                }
                Ordering::Equal => {
                    // Wait for zero
                    if sem.value != 0 {
                        sem.wait();
                        return_errno!(EAGAIN, "semaphore value not zero");
                    }
                }
                Ordering::Less => {
                    // Decrement operation
                    let op_val = -sop.sem_op as i32;
                    if sem.value < op_val {
                        sem.wait();
                        return_errno!(EAGAIN, "insufficient semaphore value");
                    }
                    sem.value -= op_val;
                }
            }
            
            sem.last_pid = current_pid;
        }

        self.sem_otime = SemManager::current_time();
        Ok(())
    }

    fn getval(&self, sem_num: usize) -> Result<i32> {
        if sem_num >= self.nsems {
            return_errno!(ERANGE, "semaphore number out of range");
        }
        Ok(self.sems[sem_num].value)
    }

    fn setval(&mut self, sem_num: usize, value: i32) -> Result<()> {
        if sem_num >= self.nsems {
            return_errno!(ERANGE, "semaphore number out of range");
        }
        if value < 0 {
            return_errno!(EINVAL, "semaphore value cannot be negative");
        }

        self.sems[sem_num].value = value;
        self.sems[sem_num].try_wake();
        self.sem_ctime = SemManager::current_time();
        Ok(())
    }
}

lazy_static! {
    pub static ref SYSTEM_V_SEM_MANAGER: SemManager = SemManager::new();
}

#[derive(Debug)]
pub struct SemManager {
    sem_sets: RwLock<HashMap<SemId, SemSet>>,
    semid_manager: RwLock<IpcIdManager>,
}

impl SemManager {
    fn new() -> Self {
        SemManager {
            sem_sets: RwLock::new(HashMap::new()),
            semid_manager: RwLock::new(IpcIdManager::new()),
        }
    }

    pub fn do_semget(&self, key: key_t, nsems: usize, semflg: SemFlags) -> Result<SemId> {
        debug!(
            "do_semget: key: {:?}, nsems: {:?}, semflg: {:?}",
            key, nsems, semflg
        );

        let mut sem_sets = self.sem_sets.write().unwrap();
        let semid = if key == IPC_PRIVATE {
            let semid = self.get_new_semid()?;
            let sem_set = SemSet::new(semid, key, nsems, 0o666)?;
            sem_sets.insert(sem_set.semid, sem_set);
            semid
        } else {
            // Find existing semaphore set
            let sem_set = sem_sets.values().find(|&set| set.key == key);
            let semid = if let Some(set) = sem_set {
                if semflg.contains(SemFlags::IPC_CREAT) && semflg.contains(SemFlags::IPC_EXCL) {
                    return_errno!(EEXIST, "semaphore set already exists");
                }
                // Check nsems matches existing set
                if nsems > 0 && nsems != set.nsems {
                    return_errno!(EINVAL, "nsems does not match existing set");
                }
                set.semid
            } else {
                if !semflg.contains(SemFlags::IPC_CREAT) {
                    return_errno!(ENOENT, "no semaphore set exists for key");
                }
                if nsems == 0 || nsems > SEMMSL {
                    return_errno!(EINVAL, "invalid nsems");
                }
                let semid = self.get_new_semid()?;
                let sem_set = SemSet::new(semid, key, nsems, 0o666)?;
                sem_sets.insert(sem_set.semid, sem_set);
                semid
            };
            semid
        };
        Ok(semid)
    }

    pub fn do_semop(&self, semid: SemId, sops_ptr: *const sembuf_t, nsops: usize) -> Result<()> {
        debug!("do_semop: semid: {:?}, nsops: {:?}", semid, nsops);
        if nsops == 0 || nsops > SEMOPM {
            return_errno!(E2BIG, "too many operations");
        }

        // Copy sembuf array from user space
        let sops = from_user::<[sembuf_t]>(sops_ptr, nsops)?;

        let pid = current!().process().pid();
        let mut sem_sets = self.sem_sets.write().unwrap();
        let sem_set = sem_sets.get_mut(&semid).ok_or_else(|| {
            errno!(EINVAL, "invalid semid")
        })?;

        sem_set.attach_pid(pid);
        sem_set.do_semop(&sops)?;
        Ok(())
    }

    pub fn do_semctl(&self, semid: SemId, semnum: usize, cmd: CmdId, arg: usize) -> Result<usize> {
        debug!(
            "do_semctl: semid: {:?}, semnum: {:?}, cmd: {:?}, arg: {:?}",
            semid, semnum, cmd, arg
        );

        let mut sem_sets = self.sem_sets.write().unwrap();
        let sem_set = sem_sets.get_mut(&semid).ok_or_else(|| {
            errno!(EINVAL, "invalid semid")
        })?;

        match cmd {
            IPC_RMID => {
                self.free_semid(&semid)?;
                sem_sets.remove(&semid);
                Ok(0)
            }
            SETVAL => {
                let value = arg as i32;
                sem_set.setval(semnum, value)?;
                Ok(0)
            }
            GETVAL => {
                let value = sem_set.getval(semnum)?;
                Ok(value as usize)
            }
            IPC_STAT => {
                let buf_ptr = arg as *mut semids_t;
                let buf = unsafe { buf_ptr.as_mut().ok_or_else(|| errno!(EFAULT, "invalid buf"))? };
                
                *buf = semids_t {
                    sem_perm: ipc_perm_t {
                        key: sem_set.key,
                        uid: sem_set.uid,
                        gid: sem_set.gid,
                        cuid: sem_set.cuid,
                        cgid: sem_set.cgid,
                        mode: sem_set.mode,
                        pad1: 0,
                        seq: 0,
                        pad2: 0,
                        unused1: 0,
                        unused2: 0,
                    },
                    sem_otime: sem_set.sem_otime,
                    sem_ctime: sem_set.sem_ctime,
                    sem_nsems: sem_set.nsems as u64,
                    unused1: 0,
                    unused2: 0,
                };
                Ok(0)
            }
            GETPID => {
                if semnum >= sem_set.nsems {
                    return_errno!(ERANGE, "semaphore number out of range");
                }
                Ok(sem_set.sems[semnum].last_pid as usize)
            }
            _ => return_errno!(EINVAL, "unsupported command"),
        }
    }

    pub fn detach_sem_when_process_exit(&self, thread: &ThreadRef) {
        let pid = thread.process().pid();
        let mut sem_sets = self.sem_sets.write().unwrap();
        
        for (_, sem_set) in sem_sets.iter_mut() {
            sem_set.detach_pid(&pid);
        }
    }

    pub fn clean_when_libos_exit(&self) {
        let mut sem_sets = self.sem_sets.write().unwrap();
        for (semid, _) in sem_sets.drain() {
            self.free_semid(&semid);
        }
    }
}

impl IpcManagerTrait for SemManager {
    fn current_time() -> time_t {
        do_gettimeofday().sec()
    }

    fn get_new_ipcid(&self) -> Result<SemId> {
        let mut semid_manager = self.semid_manager.write().unwrap();
        semid_manager.get_new_ipcid()
    }

    fn free_ipcid(&self, semid: &SemId) -> Result<()> {
        let mut semid_manager = self.semid_manager.write().unwrap();
        semid_manager.free_ipcid(semid)
    }
}