// SPDX-License-Identifier: MIT
use jfsfuse::volume::Volume;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: {} <device> <mountpoint>", args[0]);
        std::process::exit(1);
    }
    let device = &args[1];
    let mountpoint = &args[2];
    println!("Mounting JFS from {} at {}", device, mountpoint);
    let _vol = Volume::open(device)?;
    println!("JFS volume opened successfully");
    Ok(())
}
