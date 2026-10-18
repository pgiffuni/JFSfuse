// SPDX-License-Identifier: BSD-2-Clause
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use fuser::{Config, MountOption, SessionACL};
use jfsfuse::fuse::{Fuse3Fs, FuseFs};
use jfsfuse::volume::Volume;

static RUNNING: AtomicBool = AtomicBool::new(true);

extern "C" fn handle_sigint(_sig: i32) {
    RUNNING.store(false, Ordering::SeqCst);
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "usage: {} <device> <mountpoint> [--ro] [--allow-other]",
            args[0]
        );
        return ExitCode::from(1);
    }

    let device: PathBuf = args[1].clone().into();
    let mountpoint: PathBuf = args[2].clone().into();
    let read_only = args.iter().skip(3).any(|a| a == "--ro");
    let allow_other = args.iter().skip(3).any(|a| a == "--allow-other");

    let vol = if read_only {
        match Volume::open(device.to_str().unwrap()) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Error opening volume: {}", e);
                return ExitCode::from(1);
            }
        }
    } else {
        #[cfg(feature = "writable")]
        {
            match Volume::mount(device.to_str().unwrap()) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("Error mounting volume: {}", e);
                    return ExitCode::from(1);
                }
            }
        }
        #[cfg(not(feature = "writable"))]
        {
            eprintln!("Writable mount requested but filesystem built without writable feature");
            return ExitCode::from(1);
        }
    };

    #[cfg_attr(not(feature = "writable"), allow(unused_mut))]
    let mut fs = FuseFs::new(vol);
    #[cfg(feature = "writable")]
    if !read_only && let Err(e) = fs.enable_writable() {
        eprintln!("Warning: could not enable writable mode: {}", e);
    }

    let fuse_fs = Fuse3Fs::new(fs);

    let mut mount_options = Vec::new();
    mount_options.push(MountOption::FSName("jfs".to_owned()));
    if read_only {
        mount_options.push(MountOption::RO);
    }

    let mut config = Config::default();
    config.mount_options = mount_options;
    config.acl = if allow_other {
        SessionACL::All
    } else {
        SessionACL::Owner
    };
    config.n_threads = Some(4);

    let session = match fuser::spawn_mount(fuse_fs, &mountpoint, &config) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Mount failed: {}", e);
            return ExitCode::from(1);
        }
    };

    println!("Mounted JFS at {}", mountpoint.display());
    eprintln!("Press Ctrl+C to unmount.");

    unsafe {
        libc::signal(libc::SIGINT, handle_sigint as *const () as usize);
    }

    while RUNNING.load(Ordering::SeqCst) {
        std::thread::park_timeout(Duration::from_secs(1));
    }

    drop(session);
    ExitCode::SUCCESS
}
