#![cfg(unix)]

use std::fs::File;
use std::io::{self, Read};
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;

const NAME_ATTEMPTS: usize = 32;
static NAME_COUNTER: AtomicU64 = AtomicU64::new(0);
static NAME_SECRET: LazyLock<io::Result<[u8; 12]>> = LazyLock::new(|| {
    let mut secret = [0_u8; 12];
    File::open("/dev/urandom")?.read_exact(&mut secret)?;
    Ok(secret)
});

/// A POSIX shared-memory object whose mapping remains valid until the terminal
/// acknowledges that it has read the frame. Dropping it always releases only
/// this process's mapping and attempts to unlink only this object's name.
pub(crate) struct SharedFrame {
    name: [u8; 29],
    mapping: *mut u8,
    length: usize,
}

impl SharedFrame {
    pub(crate) fn create(length: usize) -> io::Result<Self> {
        if length == 0 || length > libc::off_t::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "shared-memory frame size is invalid",
            ));
        }

        for _ in 0..NAME_ATTEMPTS {
            let name = shared_memory_name(&next_name_entropy()?);
            match Self::create_named(name, length) {
                Ok(frame) => return Ok(frame),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique shared-memory frame name",
        ))
    }

    fn create_named(name: [u8; 29], length: usize) -> io::Result<Self> {
        // POSIX shm_open sets FD_CLOEXEC itself; only its documented flags belong here.
        let fd = unsafe {
            libc::shm_open(
                name.as_ptr().cast(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                0o600,
            )
        };
        if fd < 0 {
            let error = io::Error::last_os_error();
            return Err(io::Error::new(error.kind(), format!("shm_open: {error}")));
        }

        let result = Self::map(name, fd, length);
        unsafe {
            libc::close(fd);
        }
        result
    }

    fn map(name: [u8; 29], fd: RawFd, length: usize) -> io::Result<Self> {
        if unsafe { libc::ftruncate(fd, length as libc::off_t) } < 0 {
            let error = io::Error::last_os_error();
            unlink(&name);
            return Err(io::Error::new(error.kind(), format!("ftruncate: {error}")));
        }
        let mapping = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if mapping == libc::MAP_FAILED {
            let error = io::Error::last_os_error();
            unlink(&name);
            return Err(io::Error::new(error.kind(), format!("mmap: {error}")));
        }
        Ok(Self {
            name,
            mapping: mapping.cast(),
            length,
        })
    }

    pub(crate) fn name(&self) -> &[u8] {
        &self.name[..28]
    }

    pub(crate) fn bytes_mut(&mut self) -> &mut [u8] {
        // The mapping is exclusively owned by this frame. The terminal only
        // receives its name after rendering has written every byte.
        unsafe { std::slice::from_raw_parts_mut(self.mapping, self.length) }
    }
}

impl Drop for SharedFrame {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.mapping.cast(), self.length);
        }
        unlink(&self.name);
    }
}

fn unlink(name: &[u8; 29]) {
    // The Kitty protocol requires the terminal to unlink the object after it
    // has read it. ENOENT is therefore expected on the successful path.
    unsafe {
        libc::shm_unlink(name.as_ptr().cast());
    }
}

fn next_name_entropy() -> io::Result<[u8; 12]> {
    let mut random = *NAME_SECRET
        .as_ref()
        .map_err(|error| io::Error::new(error.kind(), error))?;
    let sequence = NAME_COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes();
    for (index, byte) in sequence.iter().copied().enumerate() {
        random[4 + index] ^= byte;
    }
    Ok(random)
}

fn shared_memory_name(random: &[u8; 12]) -> [u8; 29] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    // Stay below Darwin's 31-byte name limit, including the terminating NUL.
    let mut bytes = [0_u8; 29];
    bytes[..4].copy_from_slice(b"/tn-");
    for (index, value) in random.iter().copied().enumerate() {
        bytes[4 + index * 2] = HEX[(value >> 4) as usize];
        bytes[5 + index * 2] = HEX[(value & 0x0f) as usize];
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::SharedFrame;

    #[test]
    fn unacknowledged_frame_drop_removes_shared_memory() {
        let frame = SharedFrame::create(4).unwrap();
        let name = frame.name;
        let fd = unsafe { libc::shm_open(name.as_ptr().cast(), libc::O_RDONLY, 0) };
        assert!(fd >= 0, "created frame must be accessible to the terminal");
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        assert_eq!(unsafe { libc::fstat(fd, metadata.as_mut_ptr()) }, 0);
        assert_eq!(unsafe { metadata.assume_init() }.st_mode & 0o777, 0o600);
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        unsafe { libc::close(fd) };
        drop(frame);
        assert_eq!(
            unsafe { libc::shm_open(name.as_ptr().cast(), libc::O_RDONLY, 0) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ENOENT)
        );
    }

    #[test]
    fn terminal_unlink_keeps_sender_mapping_alive_until_drop() {
        let mut frame = SharedFrame::create(4).unwrap();
        frame.bytes_mut().copy_from_slice(&[21, 42, 63, 127]);
        assert_eq!(unsafe { libc::shm_unlink(frame.name.as_ptr().cast()) }, 0);
        assert_eq!(frame.bytes_mut(), &[21, 42, 63, 127]);
        drop(frame);
    }
}
