use std::{sync::atomic::Ordering, collections::BTreeMap, sync::atomic::AtomicUsize};
use std::{fmt, sync::Mutex};

#[cfg(target_os="windows")]
pub fn alloc_rwx(size: usize) -> &'static mut [u8] {
    extern {
        fn VirtualAlloc(lpAddress: *const u8, dwSize: usize,
                        flAllocationType: u32, flProtect: u32) -> *mut u8;
    }

    unsafe {
        const PAGE_EXECUTE_READWRITE: u32 = 0x40;

        const MEM_COMMIT:  u32 = 0x00001000;
        const MEM_RESERVE: u32 = 0x00002000;

        let ret = VirtualAlloc(0 as *const _, size, MEM_COMMIT | MEM_RESERVE,
                               PAGE_EXECUTE_READWRITE);
        assert!(!ret.is_null());

        std::slice::from_raw_parts_mut(ret, size)
    }
}

#[cfg(target_os="linux")]
pub fn alloc_rwx(size: usize) -> &'static mut [u8] {
    extern {
        fn mmap(addr: *mut u8, length: usize, prot: i32, flags: i32, fd: i32,
                offset: usize) -> *mut u8;
    }

    unsafe {
        // Alloc RWX and MAP_PRIVATE | MAP_ANON
        let ret = mmap(0 as *mut u8, size, 7, 34, -1, 0);
        assert!(!ret.is_null());
        
        std::slice::from_raw_parts_mut(ret, size)
    }
}


pub struct JitCache {
    /// A vector which contains the addresses of JIT code for the corresponding
    /// guest virtual address.
    ///
    /// Ex. jit_addr = jitcache.blocks[Guest Virtual Address / 4];
    ///
    /// An entry which is a zero indicates the block has not yet been
    /// translated.
    ///
    /// The blocks are referenced by the guest virtual address divided by 4
    /// because all MIPS64 instructions are 4 bytes 
    blocks: Box<[AtomicUsize]>,

    /// The raw JIT RWX backing, the amount of bytes in use, and a dedup
    /// table
    
    jit: Mutex<(&'static mut [u8], usize)>,
    //jit: Mutex<(&'static mut [u8], usize, BTreeMap<Vec<u8>, usize>)>,
    
    
    /*
    jit: &'static mut [u8],

    inuse: usize,
    */
}

impl JitCache {
    pub fn new(max_guest_addr: usize) -> Self {
        JitCache {
            blocks: (0..(max_guest_addr + 3) / 4).map(|_| {
                AtomicUsize::new(0)
            }).collect::<Vec<_>>().into_boxed_slice(),                        
            jit: Mutex::new((alloc_rwx(16 * 1024 * 1024), 0)),
        }
    }

    /// Look up the JIT address for a given guest address
    pub fn lookup(&self, addr: usize) -> Option<usize> {
        // Make sure address is aligned
        assert!(addr & 3 == 0, "Unaligned code address to JIT lookup");

        let addr = self.blocks[addr / 4].load(Ordering::SeqCst);
        if addr == 0 {
            None
        } else {
            Some(addr)
        }
    }

    pub fn add_mapping(&self, addr: usize, code: &[u8]) -> usize {
        // Make sure address is aligned
        assert!(addr & 3 == 0, "Unaligned code address to JIT lookup");

        // Get exclusive access to the JIT
        let mut jit = self.jit.lock().unwrap();

        // Now that we have the lock, check if there's already an existing mapping
        // If there is not, there is no way one could show up while we have the 
        // lock held, thus we can safely continue from this point.

        if let Some(existing) = self.lookup(addr) {
            return existing;
        } else {
            
            let jit_inuse = jit.1;

            // Number of reminaining bytes in the JIT storage
            let jit_remain = jit.0.len() - jit_inuse;
            assert!(code.len() < jit_remain, "Out of space in JIT");

            // Copy the new code into the JIT
            jit.0[jit_inuse..jit_inuse + code.len()].copy_from_slice(code);

            // Compute the address of the JIT we're inserting
            let new_addr = jit.0[jit_inuse..].as_ptr() as usize;

            // Update the JIT lookup address
            self.blocks[addr / 4].store(
                new_addr, Ordering::SeqCst);

            // Update the in use for the JIT
            jit.1 += code.len();
            
            // Return the newly allocated JIT
            new_addr
        }
    }
}

impl fmt::Display for JitCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "(Rocks)")
    }
}

impl fmt::Debug for JitCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JitCache Debug")
         .finish()
    }
}
