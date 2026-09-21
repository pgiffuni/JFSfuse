// SPDX-License-Identifier: MIT
use std::path::PathBuf;
use std::process::ExitCode;

use fuse3::raw::Session;
use jfsfuse::fuse::{Fuse3Fs, FuseFs};
use jfsfuse::volume::Volume;

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: {} <device> <mountpoint> [--ro] [--allow-other]", args[0]);
        return ExitCode::from(1);
    }

    let device: PathBuf = args[1].clone().into();
    let mountpoint: PathBuf = args[2].clone().into();
    let read_only = args.iter().skip(3).any(|a| a == "--ro");
    let allow_other = args.iter().skip(3).any(|a| a == "--allow-other");

    let vol = match Volume::open(device.to_str().unwrap()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error opening volume: {}", e);
            return ExitCode::from(1);
        }
    };

    let mut fs = FuseFs::new(vol);
    #[cfg(feature = "writable")]
    if !read_only {
        if let Err(e) = fs.enable_writable() {
            eprintln!("Warning: could not enable writable mode: {}", e);
        }
    }

    let fuse_fs = Fuse3Fs::new(fs);

    let mut mount_options = fuse3::MountOptions::default();
    mount_options.fs_name("jfs");
    if read_only {
        mount_options.read_only(true);
    }
    if allow_other {
        mount_options.allow_other(true);
    }

    let session = Session::new(mount_options);

    match session.mount(fuse_fs, &mountpoint).await {
        Ok(_handle) => {
            println!("Mounted JFS at {}", mountpoint.display());
            // The mount handle must be kept alive; dropping it unmounts.
            // For a daemon, we'd daemonize and wait on a signal.
            eprintln!("Press Ctrl+C to unmount.");
            let _ = tokio::signal::ctrl_c().await;
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("Mount failed: {}", e);
            ExitCode::from(1)
        }
    }
}
