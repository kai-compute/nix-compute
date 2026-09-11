use anyhow::Context;
use fs2::FileExt;
use std::{
    fs::{self, File, OpenOptions},
    net::TcpListener,
    path::Path,
};

pub struct Reservation {
    listener: Option<TcpListener>,
    lock: File,
    port: u16,
}

impl Reservation {
    pub fn allocate(root: &Path) -> anyhow::Result<Self> {
        fs::create_dir_all(root)?;
        for _ in 0..128 {
            let listener = TcpListener::bind(("127.0.0.1", 0))?;
            let port = listener.local_addr()?.port();
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(root.join(port.to_string()))?;
            match lock.try_lock_exclusive() {
                Ok(()) => {
                    return Ok(Self {
                        listener: Some(listener),
                        lock,
                        port,
                    })
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(error) => return Err(error).context("reserving rendezvous port"),
            }
        }
        anyhow::bail!("unable to reserve an available rendezvous port")
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn handoff(&mut self) {
        // The workload binds the socket; the file lock keeps other providers off this port.
        self.listener.take();
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.lock);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_is_reserved_until_handoff_and_claim_is_released_on_drop() {
        let root = tempfile::tempdir().unwrap();
        let mut reservation = Reservation::allocate(root.path()).unwrap();
        let port = reservation.port();
        assert!(TcpListener::bind(("127.0.0.1", port)).is_err());
        let second = Reservation::allocate(root.path()).unwrap();
        assert_ne!(port, second.port());
        reservation.handoff();
        // Another test's fork can retain the listener briefly until its exec closes it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let _workload = loop {
            match TcpListener::bind(("127.0.0.1", port)) {
                Err(error)
                    if error.kind() == std::io::ErrorKind::AddrInUse
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                result => break result.unwrap(),
            }
        };
        let claim = OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.path().join(port.to_string()))
            .unwrap();
        assert!(claim.try_lock_exclusive().is_err());
        drop(reservation);
        claim.try_lock_exclusive().unwrap();
    }
}
