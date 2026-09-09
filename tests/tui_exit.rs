#![cfg(unix)]

use std::fs::File;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct TestChild(Child);

impl Drop for TestChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn closing_the_terminal_does_not_stall_the_event_loop() {
    let directory = tempfile::tempdir().unwrap();
    let mut master_fd = -1;
    let mut slave_fd = -1;
    let mut size = libc::winsize {
        ws_row: 30,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        },
        0,
        "{}",
        std::io::Error::last_os_error()
    );
    let mut master = unsafe { File::from_raw_fd(master_fd) };
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    for file in [&master, &slave] {
        assert_eq!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
            0
        );
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_repo-sync"));
    command
        .arg("tui")
        .env("REPO_SYNC_HOME", directory.path())
        .env("TERM", "xterm-256color")
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1
                || libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) == -1
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = TestChild(command.spawn().unwrap());
    drop(command);

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut screen = Vec::new();
    while !String::from_utf8_lossy(&screen)
        .to_ascii_lowercase()
        .contains("ready")
    {
        assert!(
            Instant::now() < deadline,
            "TUI did not render its first frame"
        );
        let mut descriptor = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut descriptor, 1, 100) } > 0 {
            let mut buffer = [0; 8192];
            let count = master.read(&mut buffer).unwrap();
            assert_ne!(count, 0, "TUI closed before rendering");
            screen.extend_from_slice(&buffer[..count]);
        }
    }
    // Let the event backend enter its poll before delivering terminal EOF.
    std::thread::sleep(Duration::from_millis(100));
    drop(master);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child.0.try_wait().unwrap().is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "TUI stalled after terminal EOF");
        std::thread::sleep(Duration::from_millis(20));
    }
}
